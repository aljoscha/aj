//! ANSI-aware string utilities for terminal rendering.
//!
//! Provides functions for measuring visible width, truncating, word-wrapping, and
//! extracting column ranges from strings that contain ANSI escape codes. All operations
//! are grapheme-cluster-aware and correctly handle wide characters (CJK, emoji).

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthChar;

/// The ANSI reset sequence that clears all SGR attributes.
pub const SGR_RESET: &str = "\x1b[0m";

/// Segment reset: SGR reset + hyperlink close. Used between composited segments
/// to prevent style bleed.
pub const SEGMENT_RESET: &str = "\x1b[0m\x1b]8;;\x07";

// ---------------------------------------------------------------------------
// ANSI escape sequence extraction
// ---------------------------------------------------------------------------

/// Result of extracting an ANSI escape sequence from a string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiCode {
    /// The full escape sequence including the ESC character.
    pub code: String,
    /// The byte length consumed from the source string.
    pub byte_len: usize,
}

/// Extract an ANSI escape sequence starting at byte position `pos` in `s`.
///
/// Supports:
/// - CSI sequences (`ESC [` ... terminal byte in `mGKHJABCDdT`)
/// - OSC sequences (`ESC ]` ... `BEL` or `ST`)
/// - APC sequences (`ESC _` ... `BEL` or `ST`)
///
/// Returns `None` if the byte at `pos` is not ESC or the sequence is incomplete.
pub fn extract_ansi_code(s: &str, pos: usize) -> Option<AnsiCode> {
    let bytes = s.as_bytes();
    if pos >= bytes.len() || bytes[pos] != b'\x1b' {
        return None;
    }
    if pos + 1 >= bytes.len() {
        return None;
    }
    let next = bytes[pos + 1];

    match next {
        // CSI sequence: ESC [ <params> <terminal byte>.
        //
        // Parameter bytes are `0x30..=0x3F` (digits and `;:<=>?`);
        // intermediate bytes are `0x20..=0x2F`; final bytes are
        // `0x40..=0x7E`. We only recognize the final bytes actually
        // emitted by components and the render engine; an
        // unrecognized final byte terminates the scan and returns
        // `None` so the caller can treat the ESC as literal rather
        // than silently consuming a sequence whose shape we can't
        // validate.
        //
        // The recognized set:
        //   `m`           SGR
        //   `G`           CHA (cursor horizontal absolute)
        //   `K`           EL (erase in line)
        //   `H` `f`       CUP (cursor position)
        //   `J`           ED (erase in display)
        //   `A` `B` `C` `D` CUU/CUD/CUF/CUB (cursor up/down/fwd/back)
        //   `E` `F`       CNL/CPL (cursor next/prev line)
        //   `S` `T`       SU/SD (scroll up/down)
        //   `d`           VPA (vertical line position absolute)
        b'[' => {
            let mut j = pos + 2;
            while j < bytes.len() {
                let b = bytes[j];
                if matches!(
                    b,
                    b'm' | b'G'
                        | b'K'
                        | b'H'
                        | b'f'
                        | b'J'
                        | b'A'
                        | b'B'
                        | b'C'
                        | b'D'
                        | b'E'
                        | b'F'
                        | b'S'
                        | b'T'
                        | b'd'
                ) {
                    let code = &s[pos..=j];
                    return Some(AnsiCode {
                        code: code.to_string(),
                        byte_len: j + 1 - pos,
                    });
                }
                // Parameter (`0x30..=0x3F`) or intermediate
                // (`0x20..=0x2F`) byte: keep scanning. Anything else
                // is an unrecognized final byte; bail.
                if !matches!(b, 0x20..=0x3F) {
                    return None;
                }
                j += 1;
            }
            None
        }
        // OSC sequence: ESC ] ... BEL or ESC ] ... ESC backslash
        b']' => extract_string_terminated(s, pos),
        // APC sequence: ESC _ ... BEL or ESC _ ... ESC backslash
        b'_' => extract_string_terminated(s, pos),
        _ => None,
    }
}

/// Extract OSC or APC sequences terminated by BEL or ST (ESC \).
fn extract_string_terminated(s: &str, pos: usize) -> Option<AnsiCode> {
    let bytes = s.as_bytes();
    let mut j = pos + 2;
    while j < bytes.len() {
        if bytes[j] == b'\x07' {
            let code = &s[pos..=j];
            return Some(AnsiCode {
                code: code.to_string(),
                byte_len: j + 1 - pos,
            });
        }
        if bytes[j] == b'\x1b' && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
            let code = &s[pos..j + 2];
            return Some(AnsiCode {
                code: code.to_string(),
                byte_len: j + 2 - pos,
            });
        }
        j += 1;
    }
    None
}

// ---------------------------------------------------------------------------
// ANSI SGR state tracking
// ---------------------------------------------------------------------------

/// Tracks active ANSI SGR (Select Graphic Rendition) attributes, plus
/// the currently-open OSC 8 hyperlink if any.
///
/// Used to preserve styling across line breaks during word wrapping and
/// to reconstruct style state at arbitrary positions in styled text.
///
/// SGR attributes and hyperlink state are tracked separately because
/// their reset semantics differ at the terminal protocol level: an SGR
/// reset (`\x1b[0m`) turns colors and attributes off but does *not*
/// close an open OSC 8 hyperlink. [`Self::reset`] mirrors that split
/// (SGR-only); use [`Self::clear`] when you want both.
#[derive(Debug, Clone, Default)]
pub struct AnsiStyleTracker {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub blink: bool,
    pub inverse: bool,
    pub hidden: bool,
    pub strikethrough: bool,
    /// Foreground color code, e.g. "31" or "38;5;240" or "38;2;255;0;0".
    pub fg_color: Option<String>,
    /// Background color code, e.g. "41" or "48;5;240" or "48;2;255;0;0".
    pub bg_color: Option<String>,
    /// The URL of the currently-open OSC 8 hyperlink, or `None` if
    /// there's no open hyperlink. Opened by `\x1b]8;;URL\x1b\\` (or
    /// BEL-terminated), closed by `\x1b]8;;\x1b\\` (empty URL).
    pub active_hyperlink: Option<String>,
}

impl AnsiStyleTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Process an ANSI escape code and update the tracked state.
    ///
    /// Handles two kinds of codes:
    /// - SGR codes (`\x1b[...m`) update the attribute fields.
    /// - OSC 8 codes (`\x1b]8;<params>;<url><terminator>`) update
    ///   [`Self::active_hyperlink`].
    ///
    /// Any other code is ignored.
    pub fn process(&mut self, code: &str) {
        // OSC 8 hyperlink: ESC ] 8 ; <params> ; <url> ST-or-BEL.
        // A non-empty URL opens a hyperlink; an empty URL closes it.
        if let Some(url) = parse_osc8_url(code) {
            self.active_hyperlink = if url.is_empty() { None } else { Some(url) };
            return;
        }

        if !code.ends_with('m') {
            return;
        }
        // Extract parameters between \x1b[ and m.
        let inner = match code.strip_prefix("\x1b[").and_then(|s| s.strip_suffix('m')) {
            Some(s) => s,
            None => return,
        };
        if inner.is_empty() || inner == "0" {
            self.reset();
            return;
        }

        let parts: Vec<&str> = inner.split(';').collect();
        let mut i = 0;
        while i < parts.len() {
            let code_num: u32 = match parts[i].parse() {
                Ok(n) => n,
                Err(_) => {
                    i += 1;
                    continue;
                }
            };

            // Handle 256-color and RGB codes.
            if code_num == 38 || code_num == 48 {
                if i + 2 < parts.len() && parts[i + 1] == "5" {
                    let color_code = format!("{};{};{}", parts[i], parts[i + 1], parts[i + 2]);
                    if code_num == 38 {
                        self.fg_color = Some(color_code);
                    } else {
                        self.bg_color = Some(color_code);
                    }
                    i += 3;
                    continue;
                } else if i + 4 < parts.len() && parts[i + 1] == "2" {
                    let color_code = format!(
                        "{};{};{};{};{}",
                        parts[i],
                        parts[i + 1],
                        parts[i + 2],
                        parts[i + 3],
                        parts[i + 4]
                    );
                    if code_num == 38 {
                        self.fg_color = Some(color_code);
                    } else {
                        self.bg_color = Some(color_code);
                    }
                    i += 5;
                    continue;
                }
            }

            match code_num {
                0 => self.reset(),
                1 => self.bold = true,
                2 => self.dim = true,
                3 => self.italic = true,
                4 => self.underline = true,
                5 => self.blink = true,
                7 => self.inverse = true,
                8 => self.hidden = true,
                9 => self.strikethrough = true,
                21 => self.bold = false,
                22 => {
                    self.bold = false;
                    self.dim = false;
                }
                23 => self.italic = false,
                24 => self.underline = false,
                25 => self.blink = false,
                27 => self.inverse = false,
                28 => self.hidden = false,
                29 => self.strikethrough = false,
                39 => self.fg_color = None,
                49 => self.bg_color = None,
                c if (30..=37).contains(&c) || (90..=97).contains(&c) => {
                    self.fg_color = Some(c.to_string());
                }
                c if (40..=47).contains(&c) || (100..=107).contains(&c) => {
                    self.bg_color = Some(c.to_string());
                }
                _ => {}
            }
            i += 1;
        }
    }

    /// Reset the SGR attributes. **Leaves the hyperlink state
    /// untouched**, matching terminal semantics: an SGR reset
    /// (`\x1b[0m`) clears colors and attributes but does not close an
    /// open OSC 8 hyperlink. Use [`Self::clear`] when you want to
    /// drop the hyperlink too (e.g. when reusing a pooled tracker).
    pub fn reset(&mut self) {
        self.bold = false;
        self.dim = false;
        self.italic = false;
        self.underline = false;
        self.blink = false;
        self.inverse = false;
        self.hidden = false;
        self.strikethrough = false;
        self.fg_color = None;
        self.bg_color = None;
        // active_hyperlink intentionally preserved.
    }

    /// Clear all state, including the active hyperlink. Use this when
    /// reusing a pooled tracker between unrelated inputs.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// Build an escape sequence that re-establishes the currently
    /// active styling: an SGR block for attributes/colors followed by
    /// an OSC 8 open sequence if a hyperlink is active.
    ///
    /// Returns an empty string when no state is active.
    pub fn get_active_codes(&self) -> String {
        let mut codes: Vec<&str> = Vec::new();
        if self.bold {
            codes.push("1");
        }
        if self.dim {
            codes.push("2");
        }
        if self.italic {
            codes.push("3");
        }
        if self.underline {
            codes.push("4");
        }
        if self.blink {
            codes.push("5");
        }
        if self.inverse {
            codes.push("7");
        }
        if self.hidden {
            codes.push("8");
        }
        if self.strikethrough {
            codes.push("9");
        }
        // For color codes we need owned data, so collect differently.
        let mut parts: Vec<String> = codes.iter().map(|s| s.to_string()).collect();
        if let Some(ref fg) = self.fg_color {
            parts.push(fg.clone());
        }
        if let Some(ref bg) = self.bg_color {
            parts.push(bg.clone());
        }
        let mut result = if parts.is_empty() {
            String::new()
        } else {
            format!("\x1b[{}m", parts.join(";"))
        };
        if let Some(ref url) = self.active_hyperlink {
            result.push_str(&format!("\x1b]8;;{}\x1b\\", url));
        }
        result
    }

    /// Returns true if any attributes or a hyperlink are currently active.
    pub fn has_active_codes(&self) -> bool {
        self.bold
            || self.dim
            || self.italic
            || self.underline
            || self.blink
            || self.inverse
            || self.hidden
            || self.strikethrough
            || self.fg_color.is_some()
            || self.bg_color.is_some()
            || self.active_hyperlink.is_some()
    }

    /// Get reset codes for attributes that would bleed visually across
    /// a line break. Currently that's:
    ///
    /// - Underline (`\x1b[24m`): if left open, the underline extends
    ///   into the trailing padding of the current line on many
    ///   terminals.
    /// - Active OSC 8 hyperlink (`\x1b]8;;\x1b\\`): if left open, the
    ///   URL association carries into any subsequent cells on the
    ///   same or following row.
    ///
    /// Re-opening the hyperlink on the next line is the caller's
    /// responsibility (via [`Self::get_active_codes`]); this method
    /// only produces the closers.
    ///
    /// Returns an empty string when there's nothing to close.
    pub fn get_line_end_reset(&self) -> String {
        let mut result = String::new();
        if self.underline {
            result.push_str("\x1b[24m");
        }
        if self.active_hyperlink.is_some() {
            result.push_str("\x1b]8;;\x1b\\");
        }
        result
    }
}

/// If `code` is a well-formed OSC 8 hyperlink sequence, return the URL
/// portion (which may be empty, i.e. a close sequence). Returns `None`
/// if the code is not an OSC 8 sequence.
///
/// OSC 8 shape: `\x1b]8;<params>;<url><ST>` where `<ST>` is either
/// `\x07` (BEL) or `\x1b\\` (ESC + backslash). `<params>` is an
/// optional set of `key=value` pairs separated by `:`; `<url>` is the
/// hyperlink target (empty for a close).
fn parse_osc8_url(code: &str) -> Option<String> {
    let body = code.strip_prefix("\x1b]8;")?;
    // Strip the string terminator (BEL or ST).
    let inner = if let Some(rest) = body.strip_suffix('\x07') {
        rest
    } else {
        body.strip_suffix("\x1b\\")?
    };
    // The remaining payload is `<params>;<url>`. OSC 8 mandates at
    // least one `;` between params and url; if there's no `;`, this
    // isn't a valid OSC 8 sequence.
    let (_params, url) = inner.split_once(';')?;
    Some(url.to_string())
}

/// Scan a string and feed all ANSI codes found into a tracker.
pub fn update_tracker_from_text(text: &str, tracker: &mut AnsiStyleTracker) {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(ansi) = extract_ansi_code(text, i) {
            tracker.process(&ansi.code);
            i += ansi.byte_len;
        } else {
            // Skip to next byte. Since we're working with byte offsets and
            // extract_ansi_code only cares about ESC bytes, we just advance one byte.
            i += 1;
        }
    }
}

// ---------------------------------------------------------------------------
// Visible width calculation
// ---------------------------------------------------------------------------

/// Returns true if every byte in the string is printable ASCII (0x20..=0x7E).
fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// Calculate the terminal display width of a single grapheme cluster.
pub fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.is_empty() {
        return 0;
    }

    // Check for zero-width characters.
    let first_char = grapheme.chars().next().unwrap();
    if first_char.is_control() {
        return 0;
    }

    // Emoji detection: multi-codepoint graphemes (ZWJ sequences, skin tones)
    // or graphemes containing VS16 are typically rendered as width 2.
    let char_count = grapheme.chars().count();
    if char_count > 1 {
        // Check for variation selector 16 (emoji presentation).
        if grapheme.contains('\u{FE0F}') {
            return 2;
        }
        // Multi-codepoint sequences (ZWJ emoji, flag sequences, etc.)
        // Check if it contains combining marks or ZWJ.
        let has_zwj = grapheme.contains('\u{200D}');
        let first_cp = first_char as u32;
        let is_emoji_range = (0x1F000..=0x1FBFF).contains(&first_cp)
            || (0x2300..=0x23FF).contains(&first_cp)
            || (0x2600..=0x27BF).contains(&first_cp)
            || (0x2B50..=0x2B55).contains(&first_cp)
            || (0x1F1E6..=0x1F1FF).contains(&first_cp);
        if has_zwj || is_emoji_range {
            return 2;
        }
    }

    // Regional indicator symbols are always width 2.
    let cp = first_char as u32;
    if (0x1F1E6..=0x1F1FF).contains(&cp) {
        return 2;
    }

    // Single emoji codepoints in common emoji ranges.
    if (0x1F000..=0x1FBFF).contains(&cp)
        || (0x2600..=0x27BF).contains(&cp)
        || (0x1F900..=0x1F9FF).contains(&cp)
    {
        return 2;
    }

    // Use unicode-width for the base character.
    UnicodeWidthChar::width(first_char).unwrap_or(0)
}

/// Whether every scalar value in `g` is whitespace. Empty input returns
/// `false`.
///
/// Used by word-segmentation logic (word wrapping, Alt+word cursor
/// motion, Ctrl+W delete-word) to decide what counts as a break
/// between words. A grapheme made up of combining marks or zero-
/// width characters is *not* treated as whitespace even though its
/// visible width is zero, which keeps word deletion from swallowing
/// combining marks alongside the preceding word.
pub fn is_whitespace_grapheme(g: &str) -> bool {
    let mut saw_any = false;
    for c in g.chars() {
        if !c.is_whitespace() {
            return false;
        }
        saw_any = true;
    }
    saw_any
}

/// Whether `g` is a single ASCII-punctuation character.
///
/// The set is the classic word-segmentation punctuation bag:
/// `(){}[]<>.,;:'"!?+-=*/\|&%^$#@~`` plus backtick. Multi-scalar
/// graphemes (including emoji ZWJ sequences and any grapheme that
/// happens to start with a punctuation character but also carries a
/// combining mark) never qualify — matching word-motion behavior to
/// intuitive text-editor conventions where "word separator" is the
/// single punctuation character and nothing else.
pub fn is_punctuation_grapheme(g: &str) -> bool {
    let mut chars = g.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if chars.next().is_some() {
        return false;
    }
    matches!(
        first,
        '(' | ')'
            | '{'
            | '}'
            | '['
            | ']'
            | '<'
            | '>'
            | '.'
            | ','
            | ';'
            | ':'
            | '\''
            | '"'
            | '!'
            | '?'
            | '+'
            | '-'
            | '='
            | '*'
            | '/'
            | '\\'
            | '|'
            | '&'
            | '%'
            | '^'
            | '$'
            | '#'
            | '@'
            | '~'
            | '`'
    )
}

/// Calculate the visible terminal column width of a string, ignoring ANSI escape codes.
///
/// Handles:
/// - ANSI escape sequences (CSI, OSC, APC) -- zero width
/// - Tab characters -- counted as 3 columns
/// - Wide characters (CJK) -- counted as 2 columns
/// - Grapheme clusters (emoji, combining marks) -- proper width
pub fn visible_width(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }

    // Fast path: pure printable ASCII.
    if is_printable_ascii(s) {
        return s.len();
    }

    // Strip ANSI codes and normalize tabs, then measure graphemes.
    let mut clean = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\x1b' {
            if let Some(ansi) = extract_ansi_code(s, i) {
                i += ansi.byte_len;
                continue;
            }
        }
        if bytes[i] == b'\t' {
            clean.push_str("   ");
            i += 1;
            continue;
        }
        // Advance by one character (which may be multi-byte in UTF-8).
        if let Some(ch) = s[i..].chars().next() {
            clean.push(ch);
            i += ch.len_utf8();
        } else {
            i += 1;
        }
    }

    clean.graphemes(true).map(grapheme_width).sum()
}

// ---------------------------------------------------------------------------
// Truncation
// ---------------------------------------------------------------------------

/// Truncate a string fragment to fit within `max_width` visible columns.
/// No ellipsis is added. Returns the truncated text and its actual width.
fn truncate_fragment_to_width(text: &str, max_width: usize) -> (String, usize) {
    if max_width == 0 || text.is_empty() {
        return (String::new(), 0);
    }

    if is_printable_ascii(text) {
        if text.len() <= max_width {
            return (text.to_string(), text.len());
        }
        let clipped = &text[..max_width];
        return (clipped.to_string(), max_width);
    }

    // General case: walk graphemes, accumulating ANSI codes.
    let mut result = String::new();
    let mut pending_ansi = String::new();
    let mut width = 0_usize;
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if let Some(ansi) = extract_ansi_code(text, i) {
            pending_ansi.push_str(&ansi.code);
            i += ansi.byte_len;
            continue;
        }
        if bytes[i] == b'\t' {
            if width + 3 > max_width {
                break;
            }
            if !pending_ansi.is_empty() {
                result.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            result.push('\t');
            width += 3;
            i += 1;
            continue;
        }

        // Find the extent of non-ANSI, non-tab text.
        let text_start = i;
        let mut text_end = i;
        while text_end < bytes.len() {
            if bytes[text_end] == b'\t' {
                break;
            }
            if extract_ansi_code(text, text_end).is_some() {
                break;
            }
            if let Some(ch) = text[text_end..].chars().next() {
                text_end += ch.len_utf8();
            } else {
                text_end += 1;
            }
        }

        for grapheme in text[text_start..text_end].graphemes(true) {
            let w = grapheme_width(grapheme);
            if width + w > max_width {
                return (result, width);
            }
            if !pending_ansi.is_empty() {
                result.push_str(&pending_ansi);
                pending_ansi.clear();
            }
            result.push_str(grapheme);
            width += w;
        }
        i = text_end;
    }

    (result, width)
}

/// Truncate text to fit within `max_width` visible columns, adding an ellipsis if needed.
///
/// If `pad` is true, the result is right-padded with spaces to exactly `max_width`.
/// Properly handles ANSI escape codes (they don't count toward width).
pub fn truncate_to_width(text: &str, max_width: usize, ellipsis: &str, pad: bool) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.is_empty() {
        return if pad {
            " ".repeat(max_width)
        } else {
            String::new()
        };
    }

    let ellipsis_width = visible_width(ellipsis);

    // If ellipsis alone exceeds max_width, check if text fits; otherwise clip ellipsis.
    if ellipsis_width >= max_width {
        let text_width = visible_width(text);
        if text_width <= max_width {
            return if pad {
                let padding = max_width.saturating_sub(text_width);
                format!("{}{}", text, " ".repeat(padding))
            } else {
                text.to_string()
            };
        }
        let (clipped, clipped_width) = truncate_fragment_to_width(ellipsis, max_width);
        if clipped_width == 0 {
            return if pad {
                " ".repeat(max_width)
            } else {
                String::new()
            };
        }
        return finalize_truncated("", 0, &clipped, clipped_width, max_width, pad);
    }

    // ASCII fast path.
    if is_printable_ascii(text) {
        if text.len() <= max_width {
            return if pad {
                format!("{}{}", text, " ".repeat(max_width - text.len()))
            } else {
                text.to_string()
            };
        }
        let target_width = max_width - ellipsis_width;
        return finalize_truncated(
            &text[..target_width],
            target_width,
            ellipsis,
            ellipsis_width,
            max_width,
            pad,
        );
    }

    // General case: single-pass scan.
    let target_width = max_width - ellipsis_width;
    let mut result = String::new();
    let mut pending_ansi = String::new();
    let mut visible_so_far: usize = 0;
    let mut kept_width: usize = 0;
    let mut keep_prefix = true;
    let mut overflowed = false;

    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if let Some(ansi) = extract_ansi_code(text, i) {
            if keep_prefix {
                pending_ansi.push_str(&ansi.code);
            }
            i += ansi.byte_len;
            continue;
        }
        if bytes[i] == b'\t' {
            if keep_prefix && kept_width + 3 <= target_width {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push('\t');
                kept_width += 3;
            } else {
                keep_prefix = false;
                pending_ansi.clear();
            }
            visible_so_far += 3;
            if visible_so_far > max_width {
                overflowed = true;
                break;
            }
            i += 1;
            continue;
        }

        // Find extent of plain text.
        let text_start = i;
        let mut text_end = i;
        while text_end < bytes.len() {
            if bytes[text_end] == b'\t' {
                break;
            }
            if extract_ansi_code(text, text_end).is_some() {
                break;
            }
            if let Some(ch) = text[text_end..].chars().next() {
                text_end += ch.len_utf8();
            } else {
                text_end += 1;
            }
        }

        for grapheme in text[text_start..text_end].graphemes(true) {
            let w = grapheme_width(grapheme);
            if keep_prefix && kept_width + w <= target_width {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push_str(grapheme);
                kept_width += w;
            } else {
                keep_prefix = false;
                pending_ansi.clear();
            }
            visible_so_far += w;
            if visible_so_far > max_width {
                overflowed = true;
                break;
            }
        }
        if overflowed {
            break;
        }
        i = text_end;
    }

    let exhausted_input = i >= bytes.len();
    if !overflowed && exhausted_input {
        return if pad {
            let padding = max_width.saturating_sub(visible_so_far);
            format!("{}{}", text, " ".repeat(padding))
        } else {
            text.to_string()
        };
    }

    finalize_truncated(
        &result,
        kept_width,
        ellipsis,
        ellipsis_width,
        max_width,
        pad,
    )
}

fn finalize_truncated(
    prefix: &str,
    prefix_width: usize,
    ellipsis: &str,
    ellipsis_width: usize,
    max_width: usize,
    pad: bool,
) -> String {
    let visible = prefix_width + ellipsis_width;
    let mut out = String::with_capacity(prefix.len() + ellipsis.len() + 20);
    out.push_str(prefix);
    out.push_str(SGR_RESET);
    if !ellipsis.is_empty() {
        out.push_str(ellipsis);
        out.push_str(SGR_RESET);
    }
    if pad {
        let padding = max_width.saturating_sub(visible);
        for _ in 0..padding {
            out.push(' ');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Word wrapping
// ---------------------------------------------------------------------------

/// Split text into tokens (alternating whitespace / non-whitespace runs),
/// keeping ANSI codes attached to the following visible content.
fn split_into_tokens_with_ansi(text: &str) -> Vec<String> {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut pending_ansi = String::new();
    let mut in_whitespace = false;
    let bytes = text.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if let Some(ansi) = extract_ansi_code(text, i) {
            pending_ansi.push_str(&ansi.code);
            i += ansi.byte_len;
            continue;
        }

        let ch = text[i..].chars().next().unwrap();
        let char_is_space = ch == ' ';

        if char_is_space != in_whitespace && !current.is_empty() {
            tokens.push(std::mem::take(&mut current));
        }

        if !pending_ansi.is_empty() {
            current.push_str(&pending_ansi);
            pending_ansi.clear();
        }

        in_whitespace = char_is_space;
        current.push(ch);
        i += ch.len_utf8();
    }

    if !pending_ansi.is_empty() {
        current.push_str(&pending_ansi);
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    tokens
}

/// Break a long word that exceeds `width` into multiple lines.
/// The `tracker` is updated with any ANSI codes encountered.
fn break_long_word(word: &str, width: usize, tracker: &mut AnsiStyleTracker) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut current_line = tracker.get_active_codes();
    let mut current_width: usize = 0;

    // Segment the word into ANSI codes and grapheme clusters.
    enum Segment {
        Ansi(String),
        Grapheme(String),
    }

    let mut segments: Vec<Segment> = Vec::new();
    let bytes = word.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if let Some(ansi) = extract_ansi_code(word, i) {
            segments.push(Segment::Ansi(ansi.code.clone()));
            i += ansi.byte_len;
        } else {
            // Find extent of non-ANSI text.
            let start = i;
            let mut end = i;
            while end < bytes.len() && extract_ansi_code(word, end).is_none() {
                if let Some(ch) = word[end..].chars().next() {
                    end += ch.len_utf8();
                } else {
                    end += 1;
                }
            }
            for grapheme in word[start..end].graphemes(true) {
                segments.push(Segment::Grapheme(grapheme.to_string()));
            }
            i = end;
        }
    }

    for seg in &segments {
        match seg {
            Segment::Ansi(code) => {
                current_line.push_str(code);
                tracker.process(code);
            }
            Segment::Grapheme(g) => {
                let gw = grapheme_width(g);
                if current_width + gw > width {
                    let line_end_reset = tracker.get_line_end_reset();
                    if !line_end_reset.is_empty() {
                        current_line.push_str(&line_end_reset);
                    }
                    lines.push(std::mem::take(&mut current_line));
                    current_line = tracker.get_active_codes();
                    current_width = 0;
                }
                current_line.push_str(g);
                current_width += gw;
            }
        }
    }

    if !current_line.is_empty() {
        lines.push(current_line);
    }

    if lines.is_empty() {
        vec![String::new()]
    } else {
        lines
    }
}

/// Wrap text with ANSI codes preserved across line breaks.
///
/// Only does word wrapping -- no padding, no background colors.
/// Returns lines where each line is <= `width` visible columns.
/// Active ANSI codes are preserved across line breaks.
///
/// Handles embedded newlines by splitting on them first and tracking
/// ANSI state across the split.
pub fn wrap_text_with_ansi(text: &str, width: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let input_lines: Vec<&str> = text.split('\n').collect();
    let mut result: Vec<String> = Vec::new();
    let mut tracker = AnsiStyleTracker::new();

    for input_line in &input_lines {
        let prefix = if result.is_empty() {
            String::new()
        } else {
            tracker.get_active_codes()
        };
        let prefixed = format!("{}{}", prefix, input_line);
        result.extend(wrap_single_line(&prefixed, width));
        update_tracker_from_text(input_line, &mut tracker);
    }

    if result.is_empty() {
        vec![String::new()]
    } else {
        result
    }
}

/// Wrap a single line (no embedded newlines) to the given width.
fn wrap_single_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }

    if visible_width(line) <= width {
        return vec![line.to_string()];
    }

    let mut wrapped: Vec<String> = Vec::new();
    let mut tracker = AnsiStyleTracker::new();
    let tokens = split_into_tokens_with_ansi(line);

    let mut current_line = String::new();
    let mut current_visible_len: usize = 0;

    for token in &tokens {
        let token_visible_len = visible_width(token);
        let is_whitespace = token.trim().is_empty();

        // Token itself is too long -- break it character by character.
        if token_visible_len > width && !is_whitespace {
            if !current_line.is_empty() {
                let line_end_reset = tracker.get_line_end_reset();
                if !line_end_reset.is_empty() {
                    current_line.push_str(&line_end_reset);
                }
                wrapped.push(std::mem::take(&mut current_line));
                current_visible_len = 0;
            }

            let broken = break_long_word(token, width, &mut tracker);
            if broken.len() > 1 {
                wrapped.extend_from_slice(&broken[..broken.len() - 1]);
            }
            if let Some(last) = broken.last() {
                current_line = last.clone();
                current_visible_len = visible_width(&current_line);
            }
            continue;
        }

        // Check if adding this token would exceed width.
        let total_needed = current_visible_len + token_visible_len;

        if total_needed > width && current_visible_len > 0 {
            let mut line_to_wrap = current_line.trim_end().to_string();
            let line_end_reset = tracker.get_line_end_reset();
            if !line_end_reset.is_empty() {
                line_to_wrap.push_str(&line_end_reset);
            }
            wrapped.push(line_to_wrap);
            if is_whitespace {
                current_line = tracker.get_active_codes();
                current_visible_len = 0;
            } else {
                current_line = format!("{}{}", tracker.get_active_codes(), token);
                current_visible_len = token_visible_len;
            }
        } else {
            current_line.push_str(token);
            current_visible_len += token_visible_len;
        }

        update_tracker_from_text(token, &mut tracker);
    }

    if !current_line.is_empty() {
        wrapped.push(current_line);
    }

    if wrapped.is_empty() {
        vec![String::new()]
    } else {
        wrapped.iter().map(|l| l.trim_end().to_string()).collect()
    }
}

// ---------------------------------------------------------------------------
// Column-based slicing (for overlay compositing)
// ---------------------------------------------------------------------------

/// Extract a range of visible columns `[start_col, start_col + length)` from a line.
///
/// ANSI codes within the range are included; codes before the range are accumulated
/// and prepended when the first in-range grapheme is found. Useful for overlay compositing.
///
/// `strict` controls boundary behavior for wide characters: when `true`,
/// a grapheme whose width would make the result exceed `length` visible
/// columns is excluded (the result's visible width is guaranteed
/// `<= length`). When `false`, a wide grapheme whose *start* falls
/// inside the range is included even if its tail extends past the end.
/// Overlay compositing uses `strict = true` so a wide char straddling
/// the overlay boundary doesn't overflow the overlay's declared width.
pub fn slice_by_column(line: &str, start_col: usize, length: usize, strict: bool) -> String {
    slice_with_width(line, start_col, length, strict).0
}

/// Like `slice_by_column` but also returns the actual visible width of the result.
pub fn slice_with_width(
    line: &str,
    start_col: usize,
    length: usize,
    strict: bool,
) -> (String, usize) {
    if length == 0 {
        return (String::new(), 0);
    }
    let end_col = start_col + length;
    let mut result = String::new();
    let mut result_width: usize = 0;
    let mut current_col: usize = 0;
    let mut pending_ansi = String::new();

    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if let Some(ansi) = extract_ansi_code(line, i) {
            if current_col >= start_col && current_col < end_col {
                result.push_str(&ansi.code);
            } else if current_col < start_col {
                pending_ansi.push_str(&ansi.code);
            }
            i += ansi.byte_len;
            continue;
        }

        // Find extent of plain text.
        let text_start = i;
        let mut text_end = i;
        while text_end < bytes.len() && extract_ansi_code(line, text_end).is_none() {
            if let Some(ch) = line[text_end..].chars().next() {
                text_end += ch.len_utf8();
            } else {
                text_end += 1;
            }
        }

        for grapheme in line[text_start..text_end].graphemes(true) {
            let w = grapheme_width(grapheme);
            let in_range = current_col >= start_col && current_col < end_col;
            // `strict` excludes wide graphemes that would overflow the
            // range. Without this, a 2-wide glyph starting at
            // `end_col - 1` would be included and the result's visible
            // width would exceed `length` — fine for viewport slicing
            // where we don't care if the terminal's wrap behavior
            // clips the tail, but wrong for overlay compositing where
            // the caller is relying on the declared width to avoid
            // trampling adjacent columns.
            let fits = !strict || current_col + w <= end_col;
            if in_range && fits {
                if !pending_ansi.is_empty() {
                    result.push_str(&pending_ansi);
                    pending_ansi.clear();
                }
                result.push_str(grapheme);
                result_width += w;
            }
            current_col += w;
            if current_col >= end_col {
                break;
            }
        }
        i = text_end;
        if current_col >= end_col {
            break;
        }
    }

    (result, result_width)
}

/// Extract "before" and "after" segments from a line in a single pass.
///
/// Used for overlay compositing where we need content before and after the overlay region.
/// The "before" segment covers columns `[0, before_end)`.
/// The "after" segment covers columns `[after_start, after_start + after_len)`.
/// Styling from before the overlay gap is inherited by the "after" segment.
///
/// `strict_after` controls boundary handling on the right edge of the
/// `after` segment: when `true`, a grapheme whose left half fits at
/// `after_end - 1` but whose width would push its right half past
/// `after_end` is dropped. Pass `true` from compositors that need the
/// segment widths to honor their declared upper bound (overlay
/// boundaries); pass `false` when callers don't care that the final
/// grapheme may overshoot `after_end` by one cell.
pub fn extract_segments(
    line: &str,
    before_end: usize,
    after_start: usize,
    after_len: usize,
    strict_after: bool,
) -> (String, usize, String, usize) {
    let mut before = String::new();
    let mut before_width: usize = 0;
    let mut after = String::new();
    let mut after_width: usize = 0;
    let mut current_col: usize = 0;
    let mut pending_ansi_before = String::new();
    let mut after_started = false;
    let after_end = after_start + after_len;

    let mut style_tracker = AnsiStyleTracker::new();

    let bytes = line.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if let Some(ansi) = extract_ansi_code(line, i) {
            style_tracker.process(&ansi.code);
            if current_col < before_end {
                pending_ansi_before.push_str(&ansi.code);
            } else if current_col >= after_start && current_col < after_end && after_started {
                after.push_str(&ansi.code);
            }
            i += ansi.byte_len;
            continue;
        }

        // Find extent of plain text.
        let text_start = i;
        let mut text_end = i;
        while text_end < bytes.len() && extract_ansi_code(line, text_end).is_none() {
            if let Some(ch) = line[text_end..].chars().next() {
                text_end += ch.len_utf8();
            } else {
                text_end += 1;
            }
        }

        for grapheme in line[text_start..text_end].graphemes(true) {
            let w = grapheme_width(grapheme);

            if current_col < before_end {
                if !pending_ansi_before.is_empty() {
                    before.push_str(&pending_ansi_before);
                    pending_ansi_before.clear();
                }
                before.push_str(grapheme);
                before_width += w;
            } else if current_col >= after_start && current_col < after_end {
                // Strict boundary: drop a wide grapheme whose right half
                // would extend past `after_end`. Without this guard a
                // wide grapheme at the exact boundary leaks one column
                // past the declared after-segment width, and the
                // downstream compositor has no safe way to recover the
                // styling that was meant to terminate at `after_end`.
                let fits = !strict_after || current_col + w <= after_end;
                if fits {
                    if !after_started {
                        after.push_str(&style_tracker.get_active_codes());
                        after_started = true;
                    }
                    after.push_str(grapheme);
                    after_width += w;
                }
            }

            current_col += w;
            let done_col = if after_len == 0 {
                before_end
            } else {
                after_end
            };
            if current_col >= done_col {
                break;
            }
        }
        i = text_end;
        let done_col = if after_len == 0 {
            before_end
        } else {
            after_end
        };
        if current_col >= done_col {
            break;
        }
    }

    (before, before_width, after, after_width)
}

/// Apply a background color to a line, padding it to `width` columns.
pub fn apply_background_to_line(
    line: &str,
    width: usize,
    bg_fn: &dyn Fn(&str) -> String,
) -> String {
    let visible_len = visible_width(line);
    let padding = width.saturating_sub(visible_len);
    let with_padding = format!("{}{}", line, " ".repeat(padding));
    bg_fn(&with_padding)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- extract_ansi_code --

    #[test]
    fn test_extract_ansi_code_csi() {
        let s = "\x1b[31mhello";
        let result = extract_ansi_code(s, 0).unwrap();
        assert_eq!(result.code, "\x1b[31m");
        assert_eq!(result.byte_len, 5);
    }

    #[test]
    fn test_extract_ansi_code_csi_reset() {
        let s = "\x1b[0m";
        let result = extract_ansi_code(s, 0).unwrap();
        assert_eq!(result.code, "\x1b[0m");
    }

    #[test]
    fn test_extract_ansi_code_osc_bel() {
        let s = "\x1b]8;;https://example.com\x07link";
        let result = extract_ansi_code(s, 0).unwrap();
        assert_eq!(result.code, "\x1b]8;;https://example.com\x07");
    }

    #[test]
    fn test_extract_ansi_code_apc() {
        let s = "\x1b_cursor\x07rest";
        let result = extract_ansi_code(s, 0).unwrap();
        assert_eq!(result.code, "\x1b_cursor\x07");
    }

    #[test]
    fn test_extract_ansi_code_not_esc() {
        assert!(extract_ansi_code("hello", 0).is_none());
    }

    #[test]
    fn test_extract_ansi_code_at_offset() {
        let s = "hi\x1b[1mworld";
        let result = extract_ansi_code(s, 2).unwrap();
        assert_eq!(result.code, "\x1b[1m");
    }

    #[test]
    fn extract_ansi_code_rejects_csi_with_unrecognized_final_byte() {
        // A CSI-like sequence ending in `r` (DECSTBM, not emitted by
        // this library) must not be consumed: returning None lets the
        // caller treat the ESC as literal so visible_width counts the
        // rest correctly rather than swallowing bytes we didn't mean
        // to drop from measurement.
        assert!(extract_ansi_code("\x1b[1;24r", 0).is_none());
    }

    #[test]
    fn extract_ansi_code_accepts_all_recognized_csi_final_bytes() {
        for terminal in [
            "m", "G", "K", "H", "f", "J", "A", "B", "C", "D", "E", "F", "S", "T", "d",
        ] {
            let s = format!("\x1b[1{}rest", terminal);
            let result = extract_ansi_code(&s, 0);
            assert!(result.is_some(), "final byte {terminal} should be accepted");
            assert_eq!(result.unwrap().code, format!("\x1b[1{}", terminal));
        }
    }

    #[test]
    fn extract_ansi_code_accepts_csi_with_private_indicator() {
        // `?` in the parameter position (byte 0x3F) is used by DEC
        // private modes: `\x1b[?25h` (show cursor), `\x1b[?2026h`
        // (BSU). Must still parse when the final byte is recognized.
        let result = extract_ansi_code("\x1b[?25h", 0);
        // `h` is not in our recognized final-byte set, so this
        // specific sequence returns None — documenting the deliberate
        // scope: we only extract what the engine might emit for
        // styling/cursor work, not the full CSI grammar.
        assert!(result.is_none());
    }

    // -- AnsiStyleTracker --

    #[test]
    fn test_tracker_basic() {
        let mut t = AnsiStyleTracker::new();
        assert!(!t.has_active_codes());
        t.process("\x1b[1m"); // bold
        assert!(t.bold);
        assert_eq!(t.get_active_codes(), "\x1b[1m");
    }

    #[test]
    fn test_tracker_multiple_attrs() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b[1;3;31m"); // bold + italic + red fg
        assert!(t.bold);
        assert!(t.italic);
        assert_eq!(t.fg_color.as_deref(), Some("31"));
        assert_eq!(t.get_active_codes(), "\x1b[1;3;31m");
    }

    #[test]
    fn test_tracker_reset() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b[1;31m");
        t.process("\x1b[0m");
        assert!(!t.has_active_codes());
        assert_eq!(t.get_active_codes(), "");
    }

    #[test]
    fn test_tracker_256_color() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b[38;5;240m");
        assert_eq!(t.fg_color.as_deref(), Some("38;5;240"));
    }

    #[test]
    fn test_tracker_rgb_color() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b[48;2;255;128;0m");
        assert_eq!(t.bg_color.as_deref(), Some("48;2;255;128;0"));
    }

    #[test]
    fn test_tracker_underline_reset() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b[4m");
        assert_eq!(t.get_line_end_reset(), "\x1b[24m");
        t.process("\x1b[24m");
        assert_eq!(t.get_line_end_reset(), "");
    }

    // -- AnsiStyleTracker OSC 8 hyperlink handling --

    #[test]
    fn tracker_osc8_open_with_st_sets_active_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]8;;https://example.com\x1b\\");
        assert_eq!(t.active_hyperlink.as_deref(), Some("https://example.com"));
        assert!(t.has_active_codes());
    }

    #[test]
    fn tracker_osc8_open_with_bel_sets_active_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]8;;https://example.com\x07");
        assert_eq!(t.active_hyperlink.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn tracker_osc8_empty_url_closes_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]8;;https://example.com\x1b\\");
        t.process("\x1b]8;;\x1b\\");
        assert_eq!(t.active_hyperlink, None);
        assert!(!t.has_active_codes());
    }

    #[test]
    fn tracker_osc8_with_params_is_still_recognized() {
        let mut t = AnsiStyleTracker::new();
        // Params field (e.g. `id=anchor`) is ignored, URL is what counts.
        t.process("\x1b]8;id=anchor;https://example.com\x1b\\");
        assert_eq!(t.active_hyperlink.as_deref(), Some("https://example.com"));
    }

    #[test]
    fn tracker_sgr_reset_preserves_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b[1m");
        t.process("\x1b]8;;https://example.com\x1b\\");
        t.process("\x1b[0m"); // SGR reset
        assert!(!t.bold, "SGR reset clears bold");
        assert_eq!(
            t.active_hyperlink.as_deref(),
            Some("https://example.com"),
            "SGR reset must not close an open hyperlink",
        );
    }

    #[test]
    fn tracker_clear_drops_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]8;;https://example.com\x1b\\");
        t.clear();
        assert_eq!(t.active_hyperlink, None);
    }

    #[test]
    fn tracker_get_active_codes_appends_hyperlink_open() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b[1m");
        t.process("\x1b]8;;https://example.com\x1b\\");
        assert_eq!(
            t.get_active_codes(),
            "\x1b[1m\x1b]8;;https://example.com\x1b\\",
        );
    }

    #[test]
    fn tracker_get_active_codes_just_hyperlink_when_no_sgr() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]8;;https://example.com\x1b\\");
        assert_eq!(t.get_active_codes(), "\x1b]8;;https://example.com\x1b\\");
    }

    #[test]
    fn tracker_line_end_reset_closes_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]8;;https://example.com\x1b\\");
        assert_eq!(t.get_line_end_reset(), "\x1b]8;;\x1b\\");
    }

    #[test]
    fn tracker_line_end_reset_combines_underline_and_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b[4m");
        t.process("\x1b]8;;https://example.com\x1b\\");
        assert_eq!(t.get_line_end_reset(), "\x1b[24m\x1b]8;;\x1b\\");
    }

    #[test]
    fn tracker_non_osc8_osc_is_ignored() {
        // OSC 0 (window title) and unrelated OSC sequences must not
        // touch the hyperlink state.
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]0;window title\x07");
        assert_eq!(t.active_hyperlink, None);
    }

    // -- visible_width --

    #[test]
    fn test_visible_width_empty() {
        assert_eq!(visible_width(""), 0);
    }

    #[test]
    fn test_visible_width_ascii() {
        assert_eq!(visible_width("hello"), 5);
    }

    #[test]
    fn test_visible_width_ansi() {
        assert_eq!(visible_width("\x1b[31mhello\x1b[0m"), 5);
    }

    #[test]
    fn test_visible_width_tab() {
        assert_eq!(visible_width("\t"), 3);
    }

    #[test]
    fn test_visible_width_cjk() {
        // Each CJK character is width 2.
        assert_eq!(visible_width("\u{4F60}\u{597D}"), 4); // ni hao
        assert_eq!(visible_width("a\u{4F60}b"), 4); // a + wide + b
    }

    #[test]
    fn test_visible_width_mixed_ansi_and_text() {
        assert_eq!(visible_width("\x1b[1m\x1b[31mhi\x1b[0m there"), 8);
    }

    // -- truncate_to_width --

    #[test]
    fn test_truncate_fits() {
        assert_eq!(truncate_to_width("hello", 10, "...", false), "hello");
    }

    #[test]
    fn test_truncate_exact_fit() {
        assert_eq!(truncate_to_width("hello", 5, "...", false), "hello");
    }

    #[test]
    fn test_truncate_needed() {
        let result = truncate_to_width("hello world", 8, "...", false);
        // "hello" (5) + "..." (3) = 8
        assert_eq!(visible_width(&result), 8);
        assert!(result.contains("hello"));
        assert!(result.contains("..."));
    }

    #[test]
    fn test_truncate_with_pad() {
        let result = truncate_to_width("hi", 10, "...", true);
        assert_eq!(visible_width(&result), 10);
    }

    #[test]
    fn test_truncate_zero_width() {
        assert_eq!(truncate_to_width("hello", 0, "...", false), "");
    }

    #[test]
    fn test_truncate_empty() {
        assert_eq!(truncate_to_width("", 10, "...", false), "");
        assert_eq!(truncate_to_width("", 10, "...", true), "          ");
    }

    #[test]
    fn test_truncate_with_ansi() {
        let styled = "\x1b[31mhello world\x1b[0m";
        let result = truncate_to_width(styled, 8, "...", false);
        assert_eq!(visible_width(&result), 8);
    }

    // -- wrap_text_with_ansi --

    #[test]
    fn test_wrap_empty() {
        assert_eq!(wrap_text_with_ansi("", 80), vec![""]);
    }

    #[test]
    fn test_wrap_fits() {
        assert_eq!(wrap_text_with_ansi("hello world", 80), vec!["hello world"]);
    }

    #[test]
    fn test_wrap_basic() {
        let lines = wrap_text_with_ansi("hello world", 5);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "hello");
        assert_eq!(lines[1], "world");
    }

    #[test]
    fn test_wrap_preserves_newlines() {
        let lines = wrap_text_with_ansi("line1\nline2", 80);
        assert_eq!(lines, vec!["line1", "line2"]);
    }

    #[test]
    fn test_wrap_long_word() {
        let lines = wrap_text_with_ansi("abcdefghij", 5);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "abcde");
        assert_eq!(lines[1], "fghij");
    }

    #[test]
    fn test_wrap_with_ansi_codes() {
        let text = "\x1b[31mhello world\x1b[0m";
        let lines = wrap_text_with_ansi(text, 5);
        assert_eq!(lines.len(), 2);
        // First line should contain the red start code.
        assert!(lines[0].contains("\x1b[31m"));
        // Second line should re-establish the red color.
        assert!(lines[1].contains("\x1b[31m"));
    }

    #[test]
    fn test_wrap_preserves_ansi_across_newlines() {
        let text = "\x1b[1mhello\nworld\x1b[0m";
        let lines = wrap_text_with_ansi(text, 80);
        assert_eq!(lines.len(), 2);
        // Second line should start with bold.
        assert!(lines[1].starts_with("\x1b[1m"));
    }

    // -- slice_by_column --

    #[test]
    fn test_slice_basic() {
        assert_eq!(slice_by_column("hello world", 6, 5, false), "world");
    }

    #[test]
    fn test_slice_with_ansi() {
        let s = "\x1b[31mhello\x1b[0m world";
        // Columns 0-4 are "hello" (styled), 5 is space, 6-10 is "world".
        let result = slice_by_column(s, 0, 5, false);
        assert!(result.contains("hello"));
    }

    #[test]
    fn test_slice_empty_range() {
        assert_eq!(slice_by_column("hello", 0, 0, false), "");
    }

    #[test]
    fn test_slice_strict_excludes_wide_char_at_boundary() {
        // A double-width CJK char at column 4 of a 5-column slice
        // ("abcd" + "あ") starts inside the range but its width (2)
        // would overflow to column 6. `strict = true` drops it; the
        // result is "abcd" (width 4).
        let s = "abcdあef";
        let (strict_out, strict_w) = slice_with_width(s, 0, 5, true);
        assert_eq!(
            strict_out, "abcd",
            "strict should exclude overflowing wide char"
        );
        assert_eq!(strict_w, 4, "strict result width fits inside length");

        // Without strict, the wide char is included and the result
        // visibly exceeds the declared length.
        let (permissive_out, permissive_w) = slice_with_width(s, 0, 5, false);
        assert_eq!(permissive_out, "abcdあ");
        assert_eq!(permissive_w, 6, "permissive result width may exceed length");
    }

    // -- extract_segments --

    #[test]
    fn test_extract_segments_basic() {
        // "hello world test"
        //  01234567890123456
        // before=[0,5) = "hello", after=[12,16) = "test"
        let line = "hello world test";
        let (before, bw, after, aw) = extract_segments(line, 5, 12, 4, false);
        assert_eq!(before, "hello");
        assert_eq!(bw, 5);
        assert_eq!(after, "test");
        assert_eq!(aw, 4);
    }

    #[test]
    fn test_extract_segments_with_styling() {
        let line = "\x1b[31mhello\x1b[0m world test";
        let (before, _bw, after, _aw) = extract_segments(line, 5, 11, 4, false);
        assert!(before.contains("hello"));
        // "after" should inherit any active styling.
        assert_eq!(visible_width(&after), 4);
    }

    #[test]
    fn test_extract_segments_strict_after_drops_wide_grapheme_at_boundary() {
        // "xxx" + wide CJK + "xx"
        //  012   34          56
        // after=[3,5) covers exactly one wide grapheme (width 2).
        // With strict=true that grapheme fits (`3 + 2 <= 5`) and is
        // included. Move the boundary in by one and strict should
        // drop it.
        let line = "xxx\u{4e00}xx";
        let (_before, _bw, after, aw) = extract_segments(line, 3, 3, 2, true);
        assert_eq!(aw, 2, "wide grapheme fits exactly within [3, 5)");
        assert!(after.ends_with('\u{4e00}'));

        // Now the same input but the after segment is [3, 4) — a single
        // column. The wide grapheme's right half would extend past
        // column 4, so strict=true drops it.
        let (_before, _bw, strict_after, strict_w) = extract_segments(line, 3, 3, 1, true);
        assert_eq!(strict_w, 0, "strict=true drops the overhanging wide char");
        assert!(!strict_after.contains('\u{4e00}'));

        // With strict=false the overhang is allowed through and the
        // after segment ends up one column wider than its declared
        // length.
        let (_before, _bw, permissive_after, permissive_w) = extract_segments(line, 3, 3, 1, false);
        assert_eq!(permissive_w, 2, "strict=false lets the wide char overshoot");
        assert!(permissive_after.contains('\u{4e00}'));
    }

    // -- is_whitespace_grapheme --

    #[test]
    fn whitespace_grapheme_matches_ascii_whitespace() {
        assert!(is_whitespace_grapheme(" "));
        assert!(is_whitespace_grapheme("\t"));
        assert!(is_whitespace_grapheme("\n"));
        assert!(is_whitespace_grapheme("  \t")); // multiple whitespace scalars
        assert!(!is_whitespace_grapheme("a"));
        assert!(!is_whitespace_grapheme(""));
        // Non-breaking space counts as whitespace under `char::is_whitespace`.
        assert!(is_whitespace_grapheme("\u{00a0}"));
    }

    #[test]
    fn whitespace_grapheme_rejects_mixed_content() {
        // "a " has a non-whitespace scalar, so the whole grapheme
        // fails the check.
        assert!(!is_whitespace_grapheme("a "));
        assert!(!is_whitespace_grapheme(" a"));
    }

    // -- is_punctuation_grapheme --

    #[test]
    fn punctuation_grapheme_matches_ascii_punctuation_set() {
        for c in "(){}[]<>.,;:'\"!?+-=*/\\|&%^$#@~`".chars() {
            let s = c.to_string();
            assert!(is_punctuation_grapheme(&s), "{c:?} should be punctuation");
        }
    }

    #[test]
    fn punctuation_grapheme_rejects_non_punctuation() {
        assert!(!is_punctuation_grapheme("a"));
        assert!(!is_punctuation_grapheme("0"));
        assert!(!is_punctuation_grapheme(" "));
        assert!(!is_punctuation_grapheme(""));
    }

    #[test]
    fn punctuation_grapheme_rejects_multi_scalar_graphemes() {
        // Punctuation followed by a combining mark: starts with a
        // punctuation scalar but is not a single-scalar grapheme.
        assert!(!is_punctuation_grapheme(".\u{0301}"));
        // Emoji ZWJ sequence that happens to start with a
        // punctuation-like scalar: not punctuation for word-motion
        // purposes.
        assert!(!is_punctuation_grapheme("\u{1f1fa}\u{1f1f8}")); // 🇺🇸
    }
}
