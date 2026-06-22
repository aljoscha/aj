//! Bash-execution rendering helpers.
//!
//! Specialises [`super::tool_execution::ToolExecutionComponent`]'s
//! body rendering for the [`aj_agent::tool::ToolDetails::Bash`]
//! variant. The component itself stays generic; this module just
//! formats the stdout/stderr/exit-code triple into the line list
//! the component appends to its scrollback.
//!
//! The formatting is text-only: each non-empty channel
//! (`stdout`, `stderr`) renders under a one-line dim header, the
//! per-stream truncation marker (when present) follows the affected
//! stream, and the exit code goes underneath. Live streaming (the
//! `ToolExecutionUpdate` path that drives a 100ms debounced partial
//! snapshot) is wired through the generic component too — this helper
//! is called for every snapshot, so the on-screen body stays
//! consistent with the agent's view of the running command.

use std::path::PathBuf;

use aj_agent::tool::BashStreamTruncation;
use aj_tools::tools::bash::stream_marker;
use aj_tui::style;

use crate::modes::interactive::components::tool_execution::{HintKind, expand_hint};

/// Number of trailing lines kept per stream when rendering a
/// collapsed bash body. Mirrors `tool_execution::BASH_COLLAPSED_LINES`;
/// duplicated as a local constant rather than re-exported so the
/// callsite stays a single number to grep for.
const BASH_COLLAPSED_LINES: usize = 5;

/// Render a [`aj_agent::tool::ToolDetails::Bash`] payload to a
/// list of styled lines. The lines are append-ready: each one is
/// a complete row (no embedded newlines) carrying inline ANSI
/// escapes for any styling.
///
/// The layout matches the wire content the model sees: each stream's
/// content is followed by its truncation marker when present, the
/// exit-status indicator comes last, and a final fallback
/// `[Output truncated; full output at <path>]` line is appended only
/// when `truncated` is set without either per-stream summary — i.e.
/// when the result was persisted by an older session that lacked the
/// structured fields.
#[allow(clippy::too_many_arguments)]
pub fn render_bash_body(
    stdout: &str,
    stderr: &str,
    exit_code: Option<i32>,
    truncated: bool,
    full_output_path: Option<&PathBuf>,
    stdout_truncation: Option<&BashStreamTruncation>,
    stderr_truncation: Option<&BashStreamTruncation>,
    expanded: bool,
) -> Vec<String> {
    let mut lines = Vec::new();

    if !stdout.is_empty() {
        push_stream_lines(&mut lines, stdout, expanded);
    }
    if let Some(t) = stdout_truncation {
        lines.push(style::dim(&stream_marker(
            "stdout",
            t,
            full_output_path.map(|p| p.as_path()),
        )));
    }

    if !stderr.is_empty() {
        // Dim header so the eye notices the channel switch
        // without it competing with the actual error text.
        lines.push(style::dim("STDERR:"));
        push_stream_lines(&mut lines, stderr, expanded);
    }
    if let Some(t) = stderr_truncation {
        lines.push(style::dim(&stream_marker(
            "stderr",
            t,
            full_output_path.map(|p| p.as_path()),
        )));
    }

    if let Some(code) = exit_code {
        let label = if code == 0 {
            style::dim(&format!("[exit {code}]"))
        } else {
            style::red(&format!("[exit {code}]"))
        };
        lines.push(label);
    }

    // Legacy fallback marker: only when `truncated` is set but neither
    // structured per-stream summary is — typical of sessions captured
    // before the per-stream fields existed.
    if truncated && stdout_truncation.is_none() && stderr_truncation.is_none() {
        let marker = match full_output_path {
            Some(path) => format!("[Output truncated; full output at {}]", path.display()),
            None => "[Output truncated]".to_string(),
        };
        lines.push(style::dim(&marker));
    }

    lines
}

/// Push a stream's lines (`stdout` or `stderr`) into `out`,
/// applying tail compaction when `expanded` is false. The hint
/// describing the dropped lines is inserted before the visible
/// tail so the hint reads in the same reading direction as the
/// remaining content.
///
/// A single trailing empty element produced by `split('\n')` on a
/// stream ending in `\n` is popped first so the visible tail never
/// ends in a stray blank row and the "N earlier lines" count
/// reflects real lines. Matches the equivalent trim in
/// `render_details_body`'s `Text` / `SubAgentReport` arms.
fn push_stream_lines(out: &mut Vec<String>, stream: &str, expanded: bool) {
    let mut all_lines: Vec<&str> = stream.split('\n').collect();
    if all_lines.last().is_some_and(|l| l.is_empty()) {
        all_lines.pop();
    }
    if expanded || all_lines.len() <= BASH_COLLAPSED_LINES {
        for line in all_lines {
            out.push(line.to_string());
        }
        return;
    }
    let earlier = all_lines.len() - BASH_COLLAPSED_LINES;
    out.push(expand_hint(earlier, HintKind::Earlier));
    for line in &all_lines[all_lines.len() - BASH_COLLAPSED_LINES..] {
        out.push((*line).to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_agent::tool::TruncationCause;

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
        let lines = render_bash_body(
            "hello\nworld",
            "uh oh",
            Some(0),
            false,
            None,
            None,
            None,
            true,
        );
        let plain: Vec<_> = lines.iter().map(|s| strip_ansi(s)).collect();
        assert_eq!(
            plain,
            vec!["hello", "world", "STDERR:", "uh oh", "[exit 0]",]
        );
    }

    /// Legacy fallback path: `truncated` set without per-stream
    /// summaries (older sessions) still surfaces the path.
    #[test]
    fn surfaces_a_truncation_path_via_legacy_fallback() {
        let p = PathBuf::from("/tmp/aj-bash-xyz.log");
        let lines = render_bash_body("partial", "", Some(0), true, Some(&p), None, None, true);
        let plain: Vec<_> = lines.iter().map(|s| strip_ansi(s)).collect();
        assert!(
            plain.last().unwrap().contains("/tmp/aj-bash-xyz.log"),
            "got {:?}",
            plain
        );
        assert!(
            plain.last().unwrap().contains("Output truncated"),
            "got {:?}",
            plain
        );
    }

    /// Modern path: per-stream structured truncation produces the
    /// `[Showing lines ...]` marker placed right after the stream's
    /// content.
    #[test]
    fn renders_marker_for_stdout_truncation() {
        let p = PathBuf::from("/tmp/aj-bash-zzz.log");
        let trunc = BashStreamTruncation {
            total_lines: 5000,
            total_bytes: 5000 * 8,
            output_lines: 2000,
            output_bytes: 2000 * 8,
            truncated_by: TruncationCause::Lines,
            last_line_partial: false,
            last_line_bytes: 0,
        };
        let lines = render_bash_body(
            "line1\nline2",
            "",
            Some(0),
            true,
            Some(&p),
            Some(&trunc),
            None,
            true,
        );
        let plain: Vec<_> = lines.iter().map(|s| strip_ansi(s)).collect();
        // Marker appears between the stdout content and the exit line.
        let marker_idx = plain
            .iter()
            .position(|l| l.starts_with("[Showing lines"))
            .expect("marker present");
        let exit_idx = plain
            .iter()
            .position(|l| l.starts_with("[exit "))
            .expect("exit present");
        assert!(
            marker_idx < exit_idx,
            "marker should precede exit: {plain:?}"
        );
        assert!(
            plain[marker_idx].contains("3001-5000 of 5000"),
            "marker line: {:?}",
            plain[marker_idx]
        );
        assert!(
            plain[marker_idx].contains("stdout"),
            "marker should name the stream: {:?}",
            plain[marker_idx]
        );
    }

    #[test]
    fn omits_exit_marker_when_no_code() {
        // The agent surfaces `exit_code: None` for a cancelled or
        // timed-out run; the wire `content` already explains the
        // failure to the model, so the rendered body just shows
        // whatever the child produced before being killed.
        let lines = render_bash_body("partial output", "", None, false, None, None, None, true);
        let plain: Vec<_> = lines.iter().map(|s| strip_ansi(s)).collect();
        assert_eq!(plain, vec!["partial output"]);
    }

    /// Collapsed bash with a trailing-newline-bearing stdout (the
    /// normal shape of `echo`-style output): the synthetic empty
    /// trailing element from `split('\n')` must be popped before
    /// counting so the hint reports real lines and the visible
    /// tail doesn't end on a stray blank row.
    #[test]
    fn collapsed_bash_pops_trailing_newline_before_counting() {
        crate::config::keybindings::install_global_manager_defaults();
        // 6 real lines + trailing newline. With BASH_COLLAPSED_LINES = 5
        // we want hint = "1 earlier" and visible tail = lines 2-6.
        let stdout = "a\nb\nc\nd\ne\nf\n";
        let lines = render_bash_body(stdout, "", Some(0), false, None, None, None, false);
        let plain: Vec<_> = lines.iter().map(|s| strip_ansi(s)).collect();
        assert_eq!(
            plain,
            vec![
                "… (1 earlier lines, Alt+O to expand)".to_string(),
                "b".to_string(),
                "c".to_string(),
                "d".to_string(),
                "e".to_string(),
                "f".to_string(),
                "[exit 0]".to_string(),
            ]
        );
    }
}
