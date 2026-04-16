//! Tests for the `aj_tui::capabilities` surface and its render-time
//! integration with [`Markdown`]'s OSC 8 hyperlink emission.
//!
//! All tests run with `#[serial_test::serial]` because the
//! capabilities cache is process-wide state and they mutate both the
//! environment and the cache.

mod support;

use aj_tui::capabilities::{
    ImageProtocol, TerminalCapabilities, detect_capabilities, get_capabilities,
    reset_capabilities_cache, set_capabilities,
};
use aj_tui::component::Component;
use aj_tui::components::markdown::Markdown;

use support::strip_ansi;
use support::with_env;

/// Reset the process-wide capabilities cache and scrub every env var
/// the detector looks at, so a test starts from a known-minimal
/// environment. Returns an `EnvGuard` that restores the cleared vars
/// on drop.
fn isolated_env() -> support::env::EnvGuard {
    reset_capabilities_cache();
    with_env(&[
        ("TMUX", None),
        ("TERM", None),
        ("TERM_PROGRAM", None),
        ("COLORTERM", None),
        ("KITTY_WINDOW_ID", None),
        ("GHOSTTY_RESOURCES_DIR", None),
        ("WEZTERM_PANE", None),
        ("ITERM_SESSION_ID", None),
    ])
}

// ---------------------------------------------------------------------------
// Detection rules
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn detection_is_conservative_when_nothing_matches() {
    let _guard = isolated_env();
    let caps = detect_capabilities();
    assert!(!caps.hyperlinks);
    assert!(!caps.true_color);
    assert!(caps.images.is_none());
}

#[test]
#[serial_test::serial]
fn colorterm_truecolor_enables_true_color_but_nothing_else() {
    let _guard = isolated_env();
    let _guard2 = with_env(&[("COLORTERM", Some("truecolor"))]);
    let caps = detect_capabilities();
    assert!(caps.true_color);
    assert!(!caps.hyperlinks);
    assert!(caps.images.is_none());
}

#[test]
#[serial_test::serial]
fn tmux_disables_hyperlinks_and_images_even_with_colorterm() {
    let _guard = isolated_env();
    let _guard2 = with_env(&[
        ("TMUX", Some("/tmp/tmux-1000/default,1234,0")),
        ("COLORTERM", Some("truecolor")),
        // Even if someone tries to force Kitty detection, tmux wins.
        ("KITTY_WINDOW_ID", Some("1")),
    ]);
    let caps = detect_capabilities();
    assert!(!caps.hyperlinks, "tmux must suppress OSC 8");
    assert!(caps.images.is_none(), "tmux must suppress image protocols");
    assert!(
        caps.true_color,
        "colorterm still drives true_color under tmux"
    );
}

#[test]
#[serial_test::serial]
fn kitty_window_id_implies_kitty_graphics_and_hyperlinks() {
    let _guard = isolated_env();
    let _guard2 = with_env(&[("KITTY_WINDOW_ID", Some("1"))]);
    let caps = detect_capabilities();
    assert!(caps.hyperlinks);
    assert!(caps.true_color);
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
}

#[test]
#[serial_test::serial]
fn ghostty_env_implies_kitty_graphics() {
    let _guard = isolated_env();
    let _guard2 = with_env(&[(
        "GHOSTTY_RESOURCES_DIR",
        Some("/Applications/Ghostty.app/Contents/Resources"),
    )]);
    let caps = detect_capabilities();
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
    assert!(caps.hyperlinks);
}

#[test]
#[serial_test::serial]
fn wezterm_env_implies_kitty_graphics() {
    let _guard = isolated_env();
    let _guard2 = with_env(&[("WEZTERM_PANE", Some("0"))]);
    let caps = detect_capabilities();
    assert_eq!(caps.images, Some(ImageProtocol::Kitty));
}

#[test]
#[serial_test::serial]
fn iterm2_env_implies_iterm2_images() {
    let _guard = isolated_env();
    let _guard2 = with_env(&[("ITERM_SESSION_ID", Some("w0t0p0:ABCD"))]);
    let caps = detect_capabilities();
    assert_eq!(caps.images, Some(ImageProtocol::ITerm2));
    assert!(caps.hyperlinks);
    assert!(caps.true_color);
}

#[test]
#[serial_test::serial]
fn vscode_has_hyperlinks_and_true_color_but_no_images() {
    let _guard = isolated_env();
    let _guard2 = with_env(&[("TERM_PROGRAM", Some("vscode"))]);
    let caps = detect_capabilities();
    assert!(caps.hyperlinks);
    assert!(caps.true_color);
    assert!(caps.images.is_none());
}

#[test]
#[serial_test::serial]
fn alacritty_has_hyperlinks_and_true_color_but_no_images() {
    let _guard = isolated_env();
    let _guard2 = with_env(&[("TERM_PROGRAM", Some("alacritty"))]);
    let caps = detect_capabilities();
    assert!(caps.hyperlinks);
    assert!(caps.true_color);
    assert!(caps.images.is_none());
}

// ---------------------------------------------------------------------------
// get_capabilities caching + test overrides
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial]
fn set_capabilities_overrides_the_cache_for_subsequent_gets() {
    let _guard = isolated_env();

    // Seed the cache with a detection-based value, then override and
    // confirm the next read sees the override.
    let before = get_capabilities();
    assert!(
        !before.hyperlinks,
        "precondition: isolated env has no hyperlinks"
    );

    set_capabilities(TerminalCapabilities {
        hyperlinks: true,
        true_color: true,
        images: Some(ImageProtocol::Kitty),
    });

    let after = get_capabilities();
    assert!(after.hyperlinks);
    assert!(after.true_color);
    assert_eq!(after.images, Some(ImageProtocol::Kitty));

    // Clean up for the next test.
    reset_capabilities_cache();
}

#[test]
#[serial_test::serial]
fn reset_capabilities_cache_re_runs_detection_on_next_get() {
    let _guard = isolated_env();

    set_capabilities(TerminalCapabilities {
        hyperlinks: true,
        true_color: true,
        images: None,
    });
    assert!(get_capabilities().hyperlinks);

    reset_capabilities_cache();
    // After reset and with an isolated env, detection must go back to
    // the conservative default.
    assert!(!get_capabilities().hyperlinks);
}

// ---------------------------------------------------------------------------
// Markdown link rendering branches on capabilities.hyperlinks
// ---------------------------------------------------------------------------
//
// `Markdown::render_link` reads `get_capabilities().hyperlinks` inline
// at the link-render site, mirroring pi-tui's `markdown.ts:492` shape.
// Every test below mutates the cap cache via `set_capabilities` (or
// `isolated_env` for the conservative-default case) and asserts on the
// rendered byte stream.
//
// Mirrors pi-tui's `describe("Links")` block in
// `packages/tui/test/markdown.test.ts:1093-1198`. Two parens-fallback
// cases, three OSC 8 cases. The two `does_not_duplicate_*` tests in
// `tests/markdown.rs` are intentionally kept there because they're
// cap-state-independent (the autolink + mailto-strip branches return
// the URL once regardless of whether OSC 8 is on).

#[test]
#[serial_test::serial]
fn shows_url_in_parentheses_when_hyperlinks_are_not_supported() {
    let _guard = isolated_env();
    set_capabilities(TerminalCapabilities {
        hyperlinks: false,
        true_color: false,
        images: None,
    });

    let mut md = Markdown::new(
        "[click here](https://example.com)",
        0,
        0,
        support::themes::default_markdown_theme(),
        None,
    );
    let lines = md.render(80);
    let plain = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join(" ");

    assert!(plain.contains("click here"), "should contain link text");
    assert!(
        plain.contains("(https://example.com)"),
        "should show URL in parentheses; got {:?}",
        plain,
    );
}

#[test]
#[serial_test::serial]
fn shows_mailto_url_in_parentheses_when_hyperlinks_are_not_supported() {
    let _guard = isolated_env();
    set_capabilities(TerminalCapabilities {
        hyperlinks: false,
        true_color: false,
        images: None,
    });

    let mut md = Markdown::new(
        "[Email me](mailto:test@example.com)",
        0,
        0,
        support::themes::default_markdown_theme(),
        None,
    );
    let lines = md.render(80);
    let plain = lines
        .iter()
        .map(|l| strip_ansi(l))
        .collect::<Vec<_>>()
        .join(" ");

    assert!(plain.contains("Email me"), "should contain link text");
    assert!(
        plain.contains("(mailto:test@example.com)"),
        "should show mailto URL in parentheses; got {:?}",
        plain,
    );
}

#[test]
#[serial_test::serial]
fn emits_osc_8_hyperlink_sequence_when_terminal_supports_hyperlinks() {
    let _guard = isolated_env();
    set_capabilities(TerminalCapabilities {
        hyperlinks: true,
        true_color: false,
        images: None,
    });

    let mut md = Markdown::new(
        "[click here](https://example.com)",
        0,
        0,
        support::themes::default_markdown_theme(),
        None,
    );
    let lines = md.render(80);
    let joined = lines.join("");

    assert!(
        joined.contains("\x1b]8;;https://example.com\x1b\\"),
        "should contain OSC 8 open sequence; got {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b]8;;\x1b\\"),
        "should contain OSC 8 close sequence; got {:?}",
        joined,
    );
    // `strip_ansi` swallows OSC 8 sequences, so the visible content
    // after stripping should contain just "click here" and never the
    // URL.
    let visible = strip_ansi(&joined);
    assert!(
        visible.contains("click here"),
        "should contain link text in visible content; got {:?}",
        visible,
    );
    assert!(
        !visible.contains("https://example.com"),
        "URL must not appear inline as plain text when OSC 8 is in use; got {:?}",
        visible,
    );
}

#[test]
#[serial_test::serial]
fn uses_osc_8_for_mailto_links_when_terminal_supports_hyperlinks() {
    let _guard = isolated_env();
    set_capabilities(TerminalCapabilities {
        hyperlinks: true,
        true_color: false,
        images: None,
    });

    let mut md = Markdown::new(
        "[Email me](mailto:test@example.com)",
        0,
        0,
        support::themes::default_markdown_theme(),
        None,
    );
    let lines = md.render(80);
    let joined = lines.join("");

    assert!(
        joined.contains("\x1b]8;;mailto:test@example.com\x1b\\"),
        "should contain OSC 8 open with mailto URL; got {:?}",
        joined,
    );
    assert!(
        joined.contains("\x1b]8;;\x1b\\"),
        "should contain OSC 8 close sequence; got {:?}",
        joined,
    );
}

#[test]
#[serial_test::serial]
fn uses_osc_8_for_bare_urls_when_terminal_supports_hyperlinks() {
    let _guard = isolated_env();
    set_capabilities(TerminalCapabilities {
        hyperlinks: true,
        true_color: false,
        images: None,
    });

    let mut md = Markdown::new(
        "Visit https://example.com for more",
        0,
        0,
        support::themes::default_markdown_theme(),
        None,
    );
    let lines = md.render(80);
    let joined = lines.join("");

    assert!(
        joined.contains("\x1b]8;;https://example.com\x1b\\"),
        "should contain OSC 8 hyperlink for the bare URL; got {:?}",
        joined,
    );
}
