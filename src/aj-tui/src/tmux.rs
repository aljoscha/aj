//! Probing the tmux server for the options that decide whether aj's
//! escape sequences reach the outer terminal.
//!
//! Inside tmux, synchronized-update (DEC 2026), OSC 8 hyperlinks, and
//! DCS passthrough escapes only make it to the outer terminal when the
//! matching tmux options are on. Whether a given option is on lives in
//! the tmux *server* process — there's no env var or terminal-wire
//! query that exposes the attached client's resolved feature set — so
//! we ask the server directly via the `tmux` CLI.
//!
//! This is the crate's one sanctioned subprocess (see the crate-level
//! "prefer in-process" note). We talk to the server over its socket,
//! not the terminal wire, so it doesn't race the Tui input pipeline,
//! and [`crate::capabilities::detect_capabilities_with`] keeps the
//! probe injectable so detection stays deterministic in tests.

use std::process::Command;

/// The tmux options that govern whether aj's escape sequences reach the
/// outer terminal. Each field is the resolved state for the *attached
/// client*, not just what the config requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TmuxOptions {
    /// `sync` terminal-feature: tmux forwards synchronized-update (DEC
    /// 2026) sequences. Without it, full-screen redraws can flicker.
    pub sync: bool,
    /// `hyperlinks` terminal-feature: tmux re-emits OSC 8 hyperlinks to
    /// the outer terminal instead of stripping them.
    pub hyperlinks: bool,
    /// `allow-passthrough` option: tmux forwards DCS passthrough
    /// escapes, which OSC 52 clipboard writes ride through.
    pub allow_passthrough: bool,
}

/// Probe the running tmux server, or `None` when we're not inside tmux
/// or the server can't be queried.
///
/// Gated on `$TMUX`, which only a genuine tmux client sets and which
/// names the socket to talk to. cmux is tmux-derived but rewraps
/// escapes unreliably, so we treat it like screen and refuse to probe —
/// callers then fall back to the conservative defaults.
///
/// Not cached: it runs at most twice at startup (capability detection
/// plus the startup warning), and the derived [`crate::capabilities`]
/// result is itself cached, so the per-render path never reaches here.
pub fn options() -> Option<TmuxOptions> {
    if std::env::var_os("TMUX").is_none() || std::env::var_os("CMUX_WORKSPACE_ID").is_some() {
        return None;
    }
    // `#{client_termfeatures}` resolves the effective feature set for
    // the attached client (after `terminal-features` patterns are
    // matched against its TERM), which is what actually governs whether
    // tmux forwards our sync / hyperlink escapes. `allow-passthrough`
    // is a separate session option gating DCS passthrough.
    let termfeatures = query(&["display-message", "-p", "#{client_termfeatures}"])?;
    let allow_passthrough = query(&["show-options", "-gv", "allow-passthrough"])?;
    let features: Vec<&str> = termfeatures.split(',').map(str::trim).collect();
    Some(TmuxOptions {
        sync: features.contains(&"sync"),
        hyperlinks: features.contains(&"hyperlinks"),
        allow_passthrough: allow_passthrough.trim() == "on",
    })
}

/// Run `tmux <args>` and return its trimmed stdout, or `None` if the
/// command can't be spawned or exits non-zero.
fn query(args: &[&str]) -> Option<String> {
    let output = Command::new("tmux").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
