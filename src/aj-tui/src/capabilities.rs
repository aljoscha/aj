//! Terminal capability detection and configuration.
//!
//! Provides a small [`TerminalCapabilities`] surface that components
//! can consult to decide whether to emit optional escape sequences
//! (OSC 8 hyperlinks, true-color SGR, image protocols, etc.). The
//! detection is environment-variable based: we don't probe the
//! terminal over the wire, both to keep the library in-process only
//! (see the crate-level docs) and to avoid races with the Tui's own
//! input pipeline.
//!
//! Tests that want to exercise both code paths can reach for
//! [`set_capabilities`] to override the cached result and
//! [`reset_capabilities_cache`] to clear it between cases.
//!
//! The capability set is intentionally small — just the flags the
//! render paths actually branch on today. When a new surface appears
//! that needs to gate behind a terminal feature, add the flag here
//! rather than growing a per-component option.

use std::sync::{Mutex, OnceLock};

/// Inline-image protocol the host terminal advertises. Only one
/// protocol is supported at a time; when both are available (Kitty
/// terminals that also honor iTerm2's OSC 1337, for example) the
/// native protocol wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    /// Kitty graphics protocol (APC `\x1b_G…\x1b\\`). Emitted by Kitty,
    /// Ghostty, WezTerm, and a handful of forks.
    Kitty,
    /// iTerm2 inline images (OSC 1337). Emitted by iTerm2 itself and
    /// WezTerm's iTerm2 compatibility mode.
    ITerm2,
}

/// What the host terminal is known to support. Detected from
/// environment variables at first use and cached for the lifetime of
/// the process (test overrides aside).
///
/// Intentionally conservative defaults: when we can't positively
/// identify a feature, we leave it off so users running on unfamiliar
/// terminals see a graceful fallback rather than corrupted output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    /// OSC 8 hyperlinks. When false, components that would emit OSC 8
    /// sequences should render the URL in parentheses instead.
    pub hyperlinks: bool,
    /// 24-bit true color SGR (`\x1b[38;2;…`). Detected via
    /// `COLORTERM=truecolor` / `24bit`.
    pub true_color: bool,
    /// Inline-image protocol, or `None` when neither Kitty graphics
    /// nor iTerm2 inline images are known to be supported.
    pub images: Option<ImageProtocol>,
}

impl Default for TerminalCapabilities {
    fn default() -> Self {
        Self {
            hyperlinks: false,
            true_color: false,
            images: None,
        }
    }
}

/// Detect the host terminal's capabilities from environment variables.
///
/// Rules (applied in order; the first match wins):
///
/// 1. Running inside tmux or screen → all optional features off. Both
///    multiplexers filter or rewrap OSC 8 / image protocols in ways
///    that frequently break rendering. Passthrough is opt-in and
///    fragile; treat the safe fallback as the default.
/// 2. Kitty / Ghostty / WezTerm env vars → Kitty graphics, hyperlinks,
///    true color.
/// 3. iTerm2 env vars → iTerm2 inline images, hyperlinks, true color.
/// 4. VS Code integrated terminal → hyperlinks, true color, no images.
/// 5. Alacritty → hyperlinks, true color, no images.
/// 6. Fallback: true color if `COLORTERM` says so, everything else
///    off.
pub fn detect_capabilities() -> TerminalCapabilities {
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let color_term = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase();

    // tmux / screen short-circuit: force images off and hyperlinks off
    // even on terminals that would otherwise support them.
    let in_tmux_or_screen =
        std::env::var("TMUX").is_ok() || term.starts_with("tmux") || term.starts_with("screen");
    if in_tmux_or_screen {
        let true_color = color_term == "truecolor" || color_term == "24bit";
        return TerminalCapabilities {
            hyperlinks: false,
            true_color,
            images: None,
        };
    }

    // Kitty / Kitty-compatible terminals.
    if std::env::var("KITTY_WINDOW_ID").is_ok() || term_program == "kitty" {
        return TerminalCapabilities {
            hyperlinks: true,
            true_color: true,
            images: Some(ImageProtocol::Kitty),
        };
    }
    if term_program == "ghostty"
        || term.contains("ghostty")
        || std::env::var("GHOSTTY_RESOURCES_DIR").is_ok()
    {
        return TerminalCapabilities {
            hyperlinks: true,
            true_color: true,
            images: Some(ImageProtocol::Kitty),
        };
    }
    if std::env::var("WEZTERM_PANE").is_ok() || term_program == "wezterm" {
        return TerminalCapabilities {
            hyperlinks: true,
            true_color: true,
            images: Some(ImageProtocol::Kitty),
        };
    }

    // iTerm2.
    if std::env::var("ITERM_SESSION_ID").is_ok() || term_program == "iterm.app" {
        return TerminalCapabilities {
            hyperlinks: true,
            true_color: true,
            images: Some(ImageProtocol::ITerm2),
        };
    }

    // VS Code's integrated terminal.
    if term_program == "vscode" {
        return TerminalCapabilities {
            hyperlinks: true,
            true_color: true,
            images: None,
        };
    }

    // Alacritty.
    if term_program == "alacritty" {
        return TerminalCapabilities {
            hyperlinks: true,
            true_color: true,
            images: None,
        };
    }

    // Conservative fallback.
    let true_color = color_term == "truecolor" || color_term == "24bit";
    TerminalCapabilities {
        hyperlinks: false,
        true_color,
        images: None,
    }
}

/// Process-wide capability cache. First call to [`get_capabilities`]
/// runs [`detect_capabilities`]; later calls return the cached value.
/// [`set_capabilities`] and [`reset_capabilities_cache`] are test
/// overrides.
fn cache() -> &'static Mutex<Option<TerminalCapabilities>> {
    static CACHE: OnceLock<Mutex<Option<TerminalCapabilities>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

/// Return the current terminal capabilities, detecting (and caching)
/// them on first call. Detection is cheap (env-var lookups) but the
/// cache keeps components that consult capabilities on every render
/// from paying the cost in a loop.
pub fn get_capabilities() -> TerminalCapabilities {
    let mut guard = cache().lock().expect("capabilities cache poisoned");
    if let Some(caps) = *guard {
        return caps;
    }
    let detected = detect_capabilities();
    *guard = Some(detected);
    detected
}

/// Override the cached capabilities. Intended for tests that want to
/// exercise both code paths (with and without a given feature); the
/// override persists until [`reset_capabilities_cache`] is called or
/// another [`set_capabilities`] replaces it.
pub fn set_capabilities(caps: TerminalCapabilities) {
    let mut guard = cache().lock().expect("capabilities cache poisoned");
    *guard = Some(caps);
}

/// Clear the cached capabilities so the next [`get_capabilities`] call
/// re-runs detection. Paired with env-var mutation to probe a
/// particular detection path.
pub fn reset_capabilities_cache() {
    let mut guard = cache().lock().expect("capabilities cache poisoned");
    *guard = None;
}
