//! Tests for `wrap_text_with_ansi`: style preservation and boundary handling
//! when word-wrapping ANSI-styled text. Pure-function tests, no terminal
//! required.

use aj_tui::ansi::{visible_width, wrap_text_with_ansi};

const UNDERLINE_ON: &str = "\x1b[4m";
const UNDERLINE_OFF: &str = "\x1b[24m";
const SGR_RESET: &str = "\x1b[0m";

// ---------------------------------------------------------------------------
// Underline styling
// ---------------------------------------------------------------------------

#[test]
fn underline_does_not_apply_before_styled_text() {
    let url = "https://example.com/very/long/path/that/will/wrap";
    let text = format!("read this thread {}{}{}", UNDERLINE_ON, url, UNDERLINE_OFF);

    let wrapped = wrap_text_with_ansi(&text, 40);

    // First line is the unstyled prefix; no underline code should appear on it.
    assert_eq!(wrapped[0], "read this thread");

    // Continuation line carries the underlined URL.
    assert!(wrapped[1].starts_with(UNDERLINE_ON));
    assert!(wrapped[1].contains("https://"));
}

#[test]
fn underline_off_is_not_preceded_by_whitespace() {
    let text_with_trailing_space =
        format!("{}underlined text here {}more", UNDERLINE_ON, UNDERLINE_OFF);

    let wrapped = wrap_text_with_ansi(&text_with_trailing_space, 18);

    let bad = format!(" {}", UNDERLINE_OFF);
    assert!(
        !wrapped[0].contains(&bad),
        "first line ({:?}) has whitespace immediately before the underline-off",
        wrapped[0],
    );
}

#[test]
fn underline_is_closed_with_underline_off_not_full_reset() {
    // Middle lines carrying underlined content should close their underline
    // with `\x1b[24m`, preserving any outer styling — not slam a full reset.
    let url = "https://example.com/very/long/path/that/will/definitely/wrap";
    let text = format!("prefix {}{}{} suffix", UNDERLINE_ON, url, UNDERLINE_OFF);

    let wrapped = wrap_text_with_ansi(&text, 30);

    for line in wrapped.iter().take(wrapped.len().saturating_sub(1)).skip(1) {
        if line.contains(UNDERLINE_ON) {
            assert!(
                line.ends_with(UNDERLINE_OFF),
                "line {:?} should end with underline-off",
                line,
            );
            assert!(
                !line.ends_with(SGR_RESET),
                "line {:?} should not end with full reset",
                line,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Background color preservation
// ---------------------------------------------------------------------------

#[test]
fn background_color_is_preserved_across_wrapped_lines_without_full_reset() {
    let bg_blue = "\x1b[44m";
    let text = format!(
        "{}hello world this is blue background text{}",
        bg_blue, SGR_RESET
    );

    let wrapped = wrap_text_with_ansi(&text, 15);

    // Every line should still carry the background color.
    for line in &wrapped {
        assert!(
            line.contains(bg_blue),
            "line {:?} lost the background",
            line
        );
    }

    // Middle lines must not end with a full reset (that would kill the
    // background color for any padding the terminal paints).
    for line in wrapped.iter().take(wrapped.len().saturating_sub(1)) {
        assert!(
            !line.ends_with(SGR_RESET),
            "line {:?} should not end with full reset",
            line,
        );
    }
}

#[test]
fn wrapping_underlined_text_inside_a_background_preserves_background() {
    let text = format!(
        "\x1b[41mprefix {}UNDERLINED_CONTENT_THAT_WRAPS{} suffix{}",
        UNDERLINE_ON, UNDERLINE_OFF, SGR_RESET
    );

    let wrapped = wrap_text_with_ansi(&text, 20);

    // Every line must still carry bg color 41, either directly or merged
    // into a combined SGR (e.g. `\x1b[4;41m`).
    for line in &wrapped {
        let has_bg = line.contains("[41m") || line.contains(";41m") || line.contains("[41;");
        assert!(has_bg, "line {:?} lost background 41", line);
    }

    // Lines that opened an underline should close it with underline-off and
    // not with a full reset.
    for line in wrapped.iter().take(wrapped.len().saturating_sub(1)) {
        let opened_underline =
            (line.contains("[4m") || line.contains("[4;") || line.contains(";4m"))
                && !line.contains(UNDERLINE_OFF);
        if opened_underline {
            assert!(line.ends_with(UNDERLINE_OFF));
            assert!(!line.ends_with(SGR_RESET));
        }
    }
}

// ---------------------------------------------------------------------------
// Basic wrapping + visible width quirks
// ---------------------------------------------------------------------------

#[test]
fn wraps_plain_text_to_the_given_width() {
    let wrapped = wrap_text_with_ansi("hello world this is a test", 10);

    assert!(wrapped.len() > 1);
    for line in &wrapped {
        assert!(
            visible_width(line) <= 10,
            "line {:?} has visible width {}",
            line,
            visible_width(line),
        );
    }
}

#[test]
fn visible_width_ignores_osc_133_markers_terminated_by_bel() {
    let text = "\x1b]133;A\x07hello\x1b]133;B\x07";
    assert_eq!(visible_width(text), 5);
}

#[test]
fn visible_width_ignores_osc_sequences_terminated_by_st() {
    let text = "\x1b]133;A\x1b\\hello\x1b]133;B\x1b\\";
    assert_eq!(visible_width(text), 5);
}

#[test]
fn visible_width_treats_isolated_regional_indicators_as_width_two() {
    assert_eq!(visible_width("🇨"), 2);
    assert_eq!(visible_width("🇨🇳"), 2);
}

#[test]
fn truncates_trailing_whitespace_that_exceeds_width() {
    let wrapped = wrap_text_with_ansi("  ", 1);
    assert!(visible_width(&wrapped[0]) <= 1);
}

#[test]
fn color_codes_are_preserved_across_wrapped_lines() {
    let red = "\x1b[31m";
    let text = format!("{}hello world this is red{}", red, SGR_RESET);

    let wrapped = wrap_text_with_ansi(&text, 10);

    // Each continuation line should re-open with the red SGR.
    for line in wrapped.iter().skip(1) {
        assert!(
            line.starts_with(red),
            "continuation line {:?} missing leading red",
            line,
        );
    }

    // Middle lines must not end with a full reset.
    for line in wrapped.iter().take(wrapped.len().saturating_sub(1)) {
        assert!(!line.ends_with(SGR_RESET));
    }
}

// ---------------------------------------------------------------------------
// OSC 8 hyperlinks
// ---------------------------------------------------------------------------
//
// These tests exercise hyperlink continuity across wrap points: an
// open hyperlink whose label wraps must close at each line break and
// re-open at the start of the continuation line, otherwise the URL
// binding leaks into unrelated cells.

#[test]
fn osc_8_open_is_re_emitted_at_the_start_of_continuation_lines() {
    let url = "https://example.com";
    // OSC 8 open + 10 visible chars + OSC 8 close.
    let input = format!("\x1b]8;;{}\x1b\\0123456789\x1b]8;;\x1b\\", url);
    let lines = wrap_text_with_ansi(&input, 6);

    let open = format!("\x1b]8;;{}\x1b\\", url);

    for line in &lines {
        // Strip OSC 8 + SGR so we can tell whether the line carries
        // visible content that should be covered by the hyperlink.
        let stripped = strip_sgr(&strip_osc_8(line));
        if stripped.trim().is_empty() {
            continue;
        }
        assert!(
            line.starts_with(&open) || line.contains(&open),
            "line {:?} has visible text but no OSC 8 re-open",
            line,
        );
    }
}

#[test]
fn osc_8_is_closed_before_each_line_break() {
    let url = "https://example.com";
    let input = format!("\x1b]8;;{}\x1b\\0123456789\x1b]8;;\x1b\\", url);
    let lines = wrap_text_with_ansi(&input, 6);

    let open = format!("\x1b]8;;{}\x1b\\", url);
    let close = "\x1b]8;;\x1b\\";

    for line in lines.iter().take(lines.len().saturating_sub(1)) {
        if line.contains(&open) {
            assert!(
                line.ends_with(close),
                "non-final line {:?} is inside a hyperlink but does not close it",
                line,
            );
        }
    }
}

#[test]
fn osc_8_sequences_are_not_emitted_on_lines_outside_the_hyperlink() {
    let url = "https://example.com";
    let input = format!("before \x1b]8;;{}\x1b\\link\x1b]8;;\x1b\\ after", url,);
    let lines = wrap_text_with_ansi(&input, 80);

    // Width is large enough that nothing wraps; there should be exactly
    // one OSC 8 open and one OSC 8 close on the single output line.
    assert_eq!(lines.len(), 1);

    let expected_open = format!("\x1b]8;;{}\x1b\\", url);
    let expected_close = "\x1b]8;;\x1b\\";

    let open_count = lines[0].matches(&expected_open).count();
    let close_count = lines[0].matches(expected_close).count();

    assert_eq!(open_count, 1);
    assert_eq!(close_count, 1);
}

/// Strip every `ESC ] 8 ; ... (ESC \ | BEL)` hyperlink sequence from a
/// string. Only used by the `#[ignore]`d OSC 8 wrap tests above; kept
/// byte-oriented so it doesn't pull in a regex dependency.
#[allow(dead_code)]
fn strip_osc_8(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 3 < bytes.len() && &bytes[i..i + 4] == b"\x1b]8;" {
            let mut j = i + 4;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    j += 1;
                    break;
                }
                if bytes[j] == 0x1b && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                    j += 2;
                    break;
                }
                j += 1;
            }
            i = j;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    // `bytes` came from a `&str`, and we only dropped whole escape
    // sequences (all-ASCII), so the remainder is still valid UTF-8.
    String::from_utf8(out).expect("UTF-8 preserved through OSC-8 strip")
}

/// Strip every `ESC [ ... m` SGR sequence from a string. Same constraints
/// as `strip_osc_8`.
#[allow(dead_code)]
fn strip_sgr(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let mut j = i + 2;
            while j < bytes.len() && (bytes[j].is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'm' {
                i = j + 1;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("UTF-8 preserved through SGR strip")
}
