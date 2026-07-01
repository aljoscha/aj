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
//! detection consults to decide whether to emit OSC 8 at all), and the
//! warning assembly is frontend-agnostic and lives in
//! [`aj_app::tmux::build_warning`]. This module wires the two together:
//! probe the live tmux server, hand the result to the shared assembler.

use aj_tui::tmux::TmuxOptions;

/// Build the startup warning, or `None` when nothing needs saying.
///
/// Returns `None` when we're not inside tmux, when every option aj
/// relies on is already enabled, or when tmux can't be queried — we'd
/// rather stay silent than raise a false alarm against a tmux we failed
/// to inspect.
pub fn startup_warning() -> Option<String> {
    aj_app::tmux::build_warning(into_app_options(aj_tui::tmux::options()?))
}

/// Map the aj-tui probe result onto the shared [`aj_app::tmux::TmuxOptions`]
/// the assembler expects. The two structs carry the same three flags. They
/// stay distinct so `aj-app` never depends on `aj-tui`.
fn into_app_options(opts: TmuxOptions) -> aj_app::tmux::TmuxOptions {
    aj_app::tmux::TmuxOptions {
        sync: opts.sync,
        hyperlinks: opts.hyperlinks,
        allow_passthrough: opts.allow_passthrough,
    }
}
