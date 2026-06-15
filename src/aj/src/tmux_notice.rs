//! Startup notice for the tmux options aj's rendering relies on.
//!
//! aj draws each frame inside a synchronized-update envelope (DEC
//! private mode 2026), emits OSC 8 hyperlinks, and writes the clipboard
//! via OSC 52. Inside tmux those escapes only reach the outer terminal
//! when the matching options are enabled; otherwise the user sees
//! redraw flicker, plain-text URLs instead of links, or a clipboard
//! that never updates.
//!
//! The probe itself lives in [`aj_tui::tmux`] (it's also what capability
//! detection consults to decide whether to emit OSC 8 at all). Here we
//! turn its result into a user-facing warning naming whatever is still
//! off plus the `~/.tmux.conf` lines that fix it.

use aj_tui::tmux::TmuxOptions;

/// Build the startup warning, or `None` when nothing needs saying.
///
/// Returns `None` when we're not inside tmux, when every option aj
/// relies on is already enabled, or when tmux can't be queried — we'd
/// rather stay silent than raise a false alarm against a tmux we failed
/// to inspect.
pub fn startup_warning() -> Option<String> {
    build_warning(aj_tui::tmux::options()?)
}

/// A tmux capability aj wants on when running inside tmux.
struct Requirement {
    /// What the feature buys us, phrased for the warning.
    purpose: &'static str,
    /// The `~/.tmux.conf` line that turns it on.
    fix: &'static str,
}

/// Assemble the warning from a probed [`TmuxOptions`]. Split out from
/// [`startup_warning`] so it's unit-testable without a live tmux server.
fn build_warning(opts: TmuxOptions) -> Option<String> {
    let mut missing: Vec<Requirement> = Vec::new();
    if !opts.sync {
        missing.push(Requirement {
            purpose: "synchronized output (flicker-free redraw)",
            fix: "set -as terminal-features '*:sync'",
        });
    }
    if !opts.hyperlinks {
        missing.push(Requirement {
            purpose: "OSC 8 hyperlinks (clickable links in markdown)",
            fix: "set -as terminal-features '*:hyperlinks'",
        });
    }
    if !opts.allow_passthrough {
        missing.push(Requirement {
            purpose: "escape passthrough (clipboard via OSC 52)",
            fix: "set -g allow-passthrough on",
        });
    }

    if missing.is_empty() {
        return None;
    }

    let mut msg = String::from("Running inside tmux, but some options aj relies on are off:");
    for req in &missing {
        msg.push_str("\n  - ");
        msg.push_str(req.purpose);
    }
    msg.push_str("\nAdd to ~/.tmux.conf, then reload (tmux source-file ~/.tmux.conf):");
    for req in &missing {
        msg.push_str("\n  ");
        msg.push_str(req.fix);
    }
    Some(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(sync: bool, hyperlinks: bool, allow_passthrough: bool) -> TmuxOptions {
        TmuxOptions {
            sync,
            hyperlinks,
            allow_passthrough,
        }
    }

    #[test]
    fn nothing_missing_returns_none() {
        assert!(build_warning(opts(true, true, true)).is_none());
    }

    #[test]
    fn missing_sync_is_reported() {
        let warning = build_warning(opts(false, true, true)).expect("warning");
        assert!(warning.contains("synchronized output"));
        assert!(warning.contains("set -as terminal-features '*:sync'"));
        assert!(!warning.contains("OSC 8 hyperlinks"));
        assert!(!warning.contains("allow-passthrough"));
    }

    #[test]
    fn missing_hyperlinks_is_reported() {
        let warning = build_warning(opts(true, false, true)).expect("warning");
        assert!(warning.contains("OSC 8 hyperlinks"));
        assert!(warning.contains("set -as terminal-features '*:hyperlinks'"));
    }

    #[test]
    fn passthrough_off_is_reported() {
        let warning = build_warning(opts(true, true, false)).expect("warning");
        assert!(warning.contains("set -g allow-passthrough on"));
        assert!(!warning.contains("synchronized output"));
    }

    #[test]
    fn all_off_lists_every_fix() {
        let warning = build_warning(opts(false, false, false)).expect("warning");
        assert!(warning.contains("set -as terminal-features '*:sync'"));
        assert!(warning.contains("set -as terminal-features '*:hyperlinks'"));
        assert!(warning.contains("set -g allow-passthrough on"));
    }
}
