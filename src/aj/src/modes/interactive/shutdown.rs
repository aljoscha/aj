//! End-of-session shutdown banner: token usage summary plus
//! resume hint.
//!
//! Prints a per-agent token-usage breakdown (main + each
//! sub-agent + grand total) followed by a `Session: <id> (resume
//! with: aj continue <id>)` line so the user can pick the
//! conversation back up later.
//!
//! Both lines are printed to stdout *after* [`aj_tui::tui::Tui::stop`]
//! so the bytes land in the user's regular shell scrollback rather
//! than the alternate-screen TUI buffer that gets cleared on exit.
//! `aj_tui::style::dim` adds an ANSI dim attribute so the banner
//! sits visually below whatever the user's normal terminal output
//! looks like.
//!
//! The usage math and the pure line formatters
//! ([`build_usage_summary`], [`format_usage_summary`],
//! [`format_resume_hint`], [`format_session_usage_header`]) are
//! frontend-agnostic and live in [`aj_app::shutdown`]; they are
//! re-exported here so `aj`'s call sites keep resolving. The
//! `print_*` helpers below add the ANSI dim styling and stdout
//! rhythm, which is `aj`'s concern.

use aj_agent::types::UsageSummary;
use aj_tui::style;

pub use aj_app::shutdown::{
    build_usage_summary, build_usage_summary_from_parts, format_resume_hint,
    format_session_usage_header, format_usage_summary,
};

/// Print the end-of-session usage summary to stdout, dimmed and
/// indented to match the chat scrollback's left edge. Intended to
/// be called after [`aj_tui::tui::Tui::stop`] so the bytes land in
/// the user's regular shell scrollback.
///
/// Visual rhythm:
///
/// - A leading blank row separates the banner from the last
///   rendered TUI frame (which ends with the footer). `Tui::stop`
///   parks the cursor on the first row *immediately below* the
///   last content row, so without this blank the first `Main
///   Agent` row would butt directly against the footer.
/// - Each rendered row is prefixed with a single space so the
///   text aligns with the header (`format!(" {}", …)`), the
///   footer (same), and every chat child (`padding_x = 1`).
/// - Each row is dim-styled individually rather than wrapping the
///   whole block in a single `\x1b[2m…\x1b[22m` envelope. Per-row
///   wrapping keeps the SGR state self-contained on every line,
///   which matches how chat-scrollback notices style their text
///   and is more robust to terminals that reset attributes at
///   newline boundaries.
/// - A trailing blank gives the resume hint that may follow (or
///   the returning shell prompt) breathing room below the block.
pub fn print_usage_summary(summary: &UsageSummary) {
    print_usage_block(None, summary);
}

/// Print one session's usage block preceded by a dim
/// `Session: <id>` header line. Used when a process spans several
/// sessions and the shutdown banner itemizes each one; the shared
/// indent/dim rhythm matches [`print_usage_summary`].
pub fn print_session_usage(session_id: &str, summary: &UsageSummary) {
    print_usage_block(Some(&format_session_usage_header(session_id)), summary);
}

/// Shared printer behind [`print_usage_summary`] and
/// [`print_session_usage`]: leading blank row, optional dim header
/// line, dim usage rows, trailing blank row — all with the
/// one-space left indent that aligns with the chat scrollback.
fn print_usage_block(header: Option<&str>, summary: &UsageSummary) {
    println!();
    if let Some(header) = header {
        println!(" {}", style::dim(header));
    }
    for line in format_usage_summary(summary).lines() {
        println!(" {}", style::dim(line));
    }
    println!();
}

/// Print the resume hint to stdout, dimmed and indented. Called
/// only when the session has at least one persisted user message
/// (otherwise the hint points at an effectively-empty session and
/// isn't worth surfacing).
///
/// Shares the one-column left indent and trailing blank rhythm of
/// [`print_usage_summary`] so the two banners read as a single
/// dim end-of-session block aligned with the chat scrollback above
/// them.
pub fn print_resume_hint(session_id: &str) {
    println!(" {}", style::dim(&format_resume_hint(session_id)));
    println!();
}
