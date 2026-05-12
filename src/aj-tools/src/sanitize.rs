//! Normalise text captured from subprocesses or other tool sources
//! before it flows into the model's context or the TUI renderer.
//!
//! Tool output -- bash stdout / stderr, sub-agent reports, free-form
//! text from arbitrary tools -- routinely carries terminal-control
//! bytes whose side effects on a real terminal disagree with the
//! width math the renderer relies on. The most common offenders:
//!
//! - SGR styling escapes (`ESC [ ... m`) embedded in tool output.
//!   These are harmless on their own, but combined with reset codes
//!   and the renderer's bubble-background wrapper they leave the
//!   right edge of a tool-output bubble looking ragged when the
//!   terminal repaints surrounding cells with the default background.
//!
//! - Non-styling CSI escapes (cursor moves, erase-in-line) whose
//!   bytes the renderer cannot reliably account for and whose
//!   terminal-side effects clobber the bubble's painted cells.
//!
//! - Carriage returns from progress-style output (`...\rnew\r...`)
//!   that the renderer measures as zero-width but that the terminal
//!   honours by jumping the cursor back to column 0 and overprinting.
//!
//! - Stray C0 control bytes (`\b`, `\a`, `\f`, ...) that pad the
//!   renderer's measurement but produce surprising motion / sound
//!   in the terminal.
//!
//! [`sanitize_terminal_output`] strips all of these in one pass so
//! the renderer sees plain text where every grapheme advances the
//! cursor predictably, and the model sees a clean transcript free of
//! escape sequences it would otherwise have to parse.

/// Strip ANSI escape sequences, drop carriage returns, and remove
/// non-printable control bytes from `s`.
///
/// The output preserves UTF-8 letters / digits / punctuation, tabs
/// (`\t`), and newlines (`\n`). Everything else in the C0 range
/// (`0x00..=0x1F`) is dropped, including `\r`. DEL (`0x7F`) and the
/// "interlinear annotation" format characters (`U+FFF9..=U+FFFB`)
/// are dropped too -- the latter have crashed some width-measurement
/// libraries in the past, and stripping them at the ingress boundary
/// keeps the renderer's measurement code on a happy path.
///
/// Recognised ANSI sequence shapes:
///
/// - **CSI** (`ESC [ ...`): consumes through any final byte in
///   `0x40..=0x7E`. Covers SGR (`m`), cursor movement (`A`-`F`, `G`,
///   `H`), erase-in-line / erase-in-display (`K` / `J`), and the
///   rest of the standard CSI family.
/// - **OSC** (`ESC ] ...`): consumes through BEL (`0x07`) or ST
///   (`ESC \`). Used for window titles, hyperlinks (OSC 8), and
///   similar.
/// - **APC / PM / SOS** (`ESC _`, `ESC ^`, `ESC X`): same BEL / ST
///   termination as OSC.
/// - **Stray `ESC`**: a lone `ESC` with no recognised follower is
///   dropped, since letting it through would itself put the terminal
///   into an unexpected state.
///
/// A truncated sequence (e.g. input ends mid-CSI) consumes the
/// remainder of the input -- safer than emitting half a control
/// sequence to a downstream consumer.
pub fn sanitize_terminal_output(s: &str) -> String {
    // Fast path: pure printable ASCII with no escapes, no controls
    // worth stripping. Avoids the per-char branch tree for the
    // common case of structured tool output that's already clean.
    if is_clean_printable(s) {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            consume_escape_sequence(&mut chars);
            continue;
        }

        let code = u32::from(c);

        // Keep tab and newline; drop everything else in the C0 range.
        // This is where `\r`, `\b`, `\a`, `\f`, vertical tab, etc.
        // disappear.
        if code <= 0x1f {
            if c == '\t' || c == '\n' {
                out.push(c);
            }
            continue;
        }

        // DEL.
        if code == 0x7f {
            continue;
        }

        // Interlinear-annotation format characters that crash some
        // width-measurement libraries.
        if (0xfff9..=0xfffb).contains(&code) {
            continue;
        }

        out.push(c);
    }

    out
}

/// True iff `s` contains only printable ASCII plus tab / newline,
/// with no `\x1b` and no other C0 controls. Lets the fast path skip
/// the per-char rewrite for content that already meets the post-
/// sanitisation invariant.
fn is_clean_printable(s: &str) -> bool {
    for b in s.bytes() {
        // Multi-byte UTF-8: fall back to the slow path so we can
        // inspect the Unicode codepoint for the FFF9-FFFB range.
        if b >= 0x80 {
            return false;
        }
        if b == b'\t' || b == b'\n' {
            continue;
        }
        if b < 0x20 || b == 0x7f {
            return false;
        }
    }
    true
}

/// Consume the body of an ANSI escape sequence. The leading `ESC`
/// has already been pulled off `chars`. On entry, the next char (if
/// any) is the introducer (`[`, `]`, `_`, `^`, `X`) or something
/// else; on exit, `chars` is positioned at the byte after the
/// sequence's terminator (or at end-of-input if the sequence was
/// truncated).
fn consume_escape_sequence(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    let Some(&introducer) = chars.peek() else {
        // Lone `ESC` at end of input -- already dropped.
        return;
    };

    match introducer {
        '[' => {
            // CSI: read until a final byte in 0x40..=0x7E.
            chars.next();
            for c in chars.by_ref() {
                let code = u32::from(c);
                if (0x40..=0x7e).contains(&code) {
                    return;
                }
            }
            // Truncated CSI: input exhausted without a final byte.
            // The remainder has been consumed; nothing more to do.
        }
        ']' | '_' | '^' | 'X' => {
            // OSC / APC / PM / SOS: read until BEL or ST (`ESC \`).
            chars.next();
            while let Some(c) = chars.next() {
                if c == '\x07' {
                    return;
                }
                if c == '\x1b' {
                    // ST is `ESC \`. Eat the `\` if present.
                    if chars.peek() == Some(&'\\') {
                        chars.next();
                    }
                    return;
                }
            }
            // Truncated OSC/APC/PM/SOS.
        }
        _ => {
            // Two-byte escapes (`ESC` + single byte: `ESC =`, `ESC >`,
            // `ESC c`, etc.) are uncommon in tool output. Drop the
            // introducer along with the `ESC` and let the rest of the
            // input continue normally.
            chars.next();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_passes_through_unchanged() {
        assert_eq!(sanitize_terminal_output("hello world"), "hello world");
        assert_eq!(sanitize_terminal_output(""), "");
        assert_eq!(sanitize_terminal_output("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn utf8_text_is_preserved() {
        let s = "héllo \u{4e2d}\u{6587} \u{1F600}";
        assert_eq!(sanitize_terminal_output(s), s);
    }

    #[test]
    fn sgr_styling_escapes_are_stripped() {
        assert_eq!(
            sanitize_terminal_output("\x1b[1;31mhello\x1b[0m world"),
            "hello world",
        );
    }

    #[test]
    fn cursor_movement_escapes_are_stripped() {
        // Cursor up / down / forward / back, and erase-in-line are
        // all CSI sequences with non-`m` finals.
        assert_eq!(sanitize_terminal_output("a\x1b[3Bb"), "ab");
        assert_eq!(sanitize_terminal_output("a\x1b[Kb"), "ab");
        assert_eq!(sanitize_terminal_output("\x1b[2J\x1b[Hhome"), "home");
    }

    #[test]
    fn osc_sequences_are_stripped_for_both_terminators() {
        // BEL terminator: window title.
        assert_eq!(sanitize_terminal_output("\x1b]0;title\x07after"), "after",);
        // ST terminator: OSC 8 hyperlink open + close.
        assert_eq!(
            sanitize_terminal_output("\x1b]8;;https://x\x1b\\label\x1b]8;;\x1b\\"),
            "label",
        );
    }

    #[test]
    fn apc_pm_sos_sequences_are_stripped() {
        // APC, PM, SOS share the BEL / ST termination rule.
        assert_eq!(
            sanitize_terminal_output("before\x1b_data\x07after"),
            "beforeafter"
        );
        assert_eq!(sanitize_terminal_output("x\x1b^private\x07y"), "xy");
        assert_eq!(sanitize_terminal_output("x\x1bXsos\x1b\\y"), "xy");
    }

    #[test]
    fn carriage_returns_are_dropped() {
        // CR alone, CRLF pairs, and CR-only line endings all collapse:
        // `\r` disappears entirely so it can't reset the cursor on the
        // way to the renderer.
        assert_eq!(sanitize_terminal_output("a\rb"), "ab");
        assert_eq!(sanitize_terminal_output("a\r\nb"), "a\nb");
        assert_eq!(
            sanitize_terminal_output("progress: 10%\rprogress: 20%\n"),
            "progress: 10%progress: 20%\n",
        );
    }

    #[test]
    fn other_c0_controls_are_dropped() {
        // Backspace, bell, form feed, vertical tab, etc. all go away;
        // tab and newline survive.
        assert_eq!(sanitize_terminal_output("a\x08b\x07c\x0bd\x0ce"), "abcde");
        assert_eq!(sanitize_terminal_output("a\tb\nc"), "a\tb\nc");
    }

    #[test]
    fn del_is_dropped() {
        assert_eq!(sanitize_terminal_output("a\x7fb"), "ab");
    }

    #[test]
    fn stray_esc_with_no_follower_is_dropped() {
        assert_eq!(sanitize_terminal_output("a\x1b"), "a");
    }

    #[test]
    fn two_byte_escape_drops_both_bytes() {
        // `ESC c` (full reset) and similar fall through the "unknown
        // introducer" branch and lose both the ESC and the follower.
        assert_eq!(sanitize_terminal_output("a\x1bcb"), "ab");
        assert_eq!(sanitize_terminal_output("a\x1b=b"), "ab");
    }

    #[test]
    fn truncated_csi_consumes_to_end_of_input() {
        // A CSI sequence that runs off the end of the input is dropped
        // entirely. Better than emitting `ESC [ 1 ; 3` to a downstream
        // consumer.
        assert_eq!(sanitize_terminal_output("a\x1b[1;3"), "a");
    }

    #[test]
    fn truncated_osc_consumes_to_end_of_input() {
        assert_eq!(sanitize_terminal_output("a\x1b]0;title"), "a");
    }

    #[test]
    fn interlinear_annotation_chars_are_dropped() {
        let s = "a\u{fff9}b\u{fffa}c\u{fffb}d";
        assert_eq!(sanitize_terminal_output(s), "abcd");
    }

    #[test]
    fn realistic_cargo_style_output_strips_all_terminal_controls() {
        // Realistic mixed input: SGR styling around content, a CR-
        // based overprint, and a stray erase-in-line. After
        // sanitisation: no ESC, no CR, just plain text.
        let input = "\x1b[1mwarning\x1b[0m: unused\r\x1b[Klines\n";
        let out = sanitize_terminal_output(input);
        assert!(!out.contains('\x1b'), "no ESC in {out:?}");
        assert!(!out.contains('\r'), "no CR in {out:?}");
        assert_eq!(out, "warning: unusedlines\n");
    }
}
