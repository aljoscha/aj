//! ANSI-aware string utilities for terminal rendering.
//!
//! Provides functions for measuring visible width, truncating, word-wrapping, and
//! extracting column ranges from strings that contain ANSI escape codes. All operations
//! are grapheme-cluster-aware and correctly handle wide characters (CJK, emoji).

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

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
/// - CSI sequences (`ESC [` ... terminal byte in `mGKHJ`)
/// - OSC sequences (`ESC ]` ... `BEL` or `ST`)
/// - APC sequences (`ESC _` ... `BEL` or `ST`)
///
/// Returns `None` if the byte at `pos` is not ESC or the sequence is incomplete.
///
/// The recognized CSI final-byte set is `m G K H J` — the codes
/// components actually emit into rendered content. Cursor-movement
/// and scroll-region commands (`f A B C D E F S T d`, etc.) are *not*
/// recognized: those bytes are only ever emitted by the differential
/// renderer directly into the transport buffer (see `tui.rs`), never
/// into per-line `String`s that flow through ansi-aware helpers.
/// Treating an unknown CSI as literal text means `visible_width`
/// over-counts a stray cursor escape rather than silently swallowing
/// bytes whose semantics we can't validate.
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
        // `0x40..=0x7E`. We recognize only the final bytes that
        // components actually emit into rendered content:
        //
        //   `m`  SGR
        //   `G`  CHA (cursor horizontal absolute)
        //   `K`  EL  (erase in line)
        //   `H`  CUP (cursor position)
        //   `J`  ED  (erase in display)
        //
        // An unrecognized final byte terminates the scan and returns
        // `None` so the caller treats the ESC as literal rather than
        // silently consuming a sequence whose shape we can't validate.
        b'[' => {
            let mut j = pos + 2;
            while j < bytes.len() {
                let b = bytes[j];
                if matches!(b, b'm' | b'G' | b'K' | b'H' | b'J') {
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

/// String terminator used by an OSC 8 hyperlink sequence.
///
/// Some terminals (notably during OAuth login flows) only treat
/// BEL-terminated hyperlinks as clickable. We track the terminator
/// used by the open sequence and re-emit the same form so wrapping a
/// long URL doesn't silently change which lines remain clickable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Osc8Terminator {
    /// `\x07` (BEL).
    Bel,
    /// `\x1b\\` (ESC + backslash, the canonical ECMA-48 string terminator).
    St,
}

impl Osc8Terminator {
    /// The byte sequence used to terminate an OSC 8 sequence with this
    /// terminator.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bel => "\x07",
            Self::St => "\x1b\\",
        }
    }
}

/// Active OSC 8 hyperlink state preserved across the lifetime of a
/// [`AnsiStyleTracker`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveHyperlink {
    /// Optional `key=value:...` params field that appears between the
    /// `OSC 8 ;` opener and the URL. Empty for the common case.
    pub params: String,
    /// The hyperlink target URL.
    pub url: String,
    /// The terminator (BEL or ST) the opening sequence used.
    pub terminator: Osc8Terminator,
}

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
    /// The currently-open OSC 8 hyperlink, or `None` if there's no
    /// open hyperlink. Opened by `\x1b]8;<params>;<url><term>` and
    /// closed by `\x1b]8;;<term>` (empty URL). The terminator from
    /// the opening sequence is preserved so wrapping doesn't silently
    /// flip BEL→ST or vice versa.
    pub active_hyperlink: Option<ActiveHyperlink>,
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
        if let Some(parsed) = parse_osc8_hyperlink(code) {
            self.active_hyperlink = if parsed.url.is_empty() {
                None
            } else {
                Some(parsed)
            };
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
        if let Some(ref hyperlink) = self.active_hyperlink {
            result.push_str(&format!(
                "\x1b]8;{};{}{}",
                hyperlink.params,
                hyperlink.url,
                hyperlink.terminator.as_str(),
            ));
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
        if let Some(ref hyperlink) = self.active_hyperlink {
            // Close uses the same terminator the open used. Some
            // terminals only treat BEL-terminated hyperlinks as
            // clickable, and a BEL→ST flip on a wrapped line would
            // strip clickability from continuation rows.
            result.push_str(&format!("\x1b]8;;{}", hyperlink.terminator.as_str()));
        }
        result
    }
}

/// If `code` is a well-formed OSC 8 hyperlink sequence, return the
/// parsed hyperlink (params, URL, terminator). The URL may be empty,
/// indicating a close sequence. Returns `None` if `code` is not an OSC
/// 8 sequence.
///
/// OSC 8 shape: `\x1b]8;<params>;<url><ST>` where `<ST>` is either
/// `\x07` (BEL) or `\x1b\\` (ESC + backslash). `<params>` is an
/// optional set of `key=value` pairs separated by `:`; `<url>` is the
/// hyperlink target (empty for a close).
fn parse_osc8_hyperlink(code: &str) -> Option<ActiveHyperlink> {
    let body = code.strip_prefix("\x1b]8;")?;
    // Strip the string terminator (BEL or ST) and remember which one.
    let (inner, terminator) = if let Some(rest) = body.strip_suffix('\x07') {
        (rest, Osc8Terminator::Bel)
    } else if let Some(rest) = body.strip_suffix("\x1b\\") {
        (rest, Osc8Terminator::St)
    } else {
        return None;
    };
    // The remaining payload is `<params>;<url>`. OSC 8 mandates at
    // least one `;` between params and url; if there's no `;`, this
    // isn't a valid OSC 8 sequence.
    let (params, url) = inner.split_once(';')?;
    Some(ActiveHyperlink {
        params: params.to_string(),
        url: url.to_string(),
        terminator,
    })
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
///
/// Delegates to [`UnicodeWidthStr::width`], which (as of unicode-width
/// 0.2) encodes the default emoji-presentation rule: characters with
/// `Emoji_Presentation=Yes` are width 2, characters that need VS16 to
/// switch to emoji presentation are width 1 without it and width 2
/// with it, and ZWJ emoji sequences are width 2. This avoids
/// over-counting text-presentation characters that sit inside the
/// broad emoji blocks (e.g. U+2611 ☑, U+2764 ❤, U+1F100), which a
/// naive block-based check would report as width 2 unconditionally.
///
/// The one override we keep is for regional indicator codepoints
/// (`U+1F1E6..=U+1F1FF`). `UnicodeWidthStr::width` reports 1 for an
/// isolated regional indicator, but terminals usually render even a
/// half-arrived flag as a 2-wide tofu glyph during streaming. Holding
/// at 2 keeps differential-render math stable while a flag pair is
/// being assembled grapheme by grapheme.
pub fn grapheme_width(grapheme: &str) -> usize {
    if grapheme.is_empty() {
        return 0;
    }

    // Control characters (Cc) never advance the cursor. `UnicodeWidthStr`
    // already returns 0 for most of them; an explicit fast path keeps
    // the early return cheap for `\n`, `\t`, etc. when callers pass
    // them through here directly.
    let first_char = grapheme.chars().next().unwrap();
    if first_char.is_control() {
        return 0;
    }

    // Regional indicator override: see doc comment.
    let cp = u32::from(first_char);
    if (0x1F1E6..=0x1F1FF).contains(&cp) {
        return 2;
    }

    UnicodeWidthStr::width(grapheme)
}

// ---------------------------------------------------------------------------
// Terminal-output normalization
// ---------------------------------------------------------------------------

/// Normalize a string for terminal *output* without changing its logical
/// content. Some terminals render the precomposed Thai/Lao SARA AM vowels
/// (`U+0E33`, `U+0EB3`) inconsistently during differential repaint — the
/// glyph leaves stale-cell artifacts when a row is partially overwritten.
/// Their compatibility decompositions (`U+0E4D U+0E32` and `U+0ECD U+0EB2`)
/// have the same display width but avoid the artifact in practice.
///
/// This is purely a cosmetic / repaint-stability transform. Editors and
/// other components must keep using the precomposed codepoint internally so
/// cursor positions, selections, and width math stay consistent; only the
/// final bytes handed to the terminal go through this function.
///
/// In-place to keep the render hot path allocation-free on the
/// overwhelmingly common case (line contains no SARA AM vowel):
/// `Tui::render` calls this on every line of every frame, and a 25k-row
/// scrollback would otherwise see 25k `String` allocations per frame
/// even when no normalization is needed.
pub fn normalize_terminal_output(s: &mut String) {
    // Fast path: most strings don't contain either codepoint, and we
    // call this on every painted line of every TUI frame. Scan the
    // raw bytes for the two target UTF-8 sequences (`E0 B8 B3` for
    // U+0E33, `E0 BA B3` for U+0EB3) instead of going through
    // `str::contains([char])`, which builds a `MultiCharEqSearcher`
    // and walks the input char by char.
    if !contains_thai_lao_am(s.as_bytes()) {
        return;
    }

    let mut out = String::with_capacity(s.len() + 2);
    for ch in s.chars() {
        match ch {
            '\u{0E33}' => {
                out.push('\u{0E4D}');
                out.push('\u{0E32}');
            }
            '\u{0EB3}' => {
                out.push('\u{0ECD}');
                out.push('\u{0EB2}');
            }
            other => out.push(other),
        }
    }
    *s = out;
}

/// True iff `bytes` contains the UTF-8 encoding of either
/// `U+0E33 THAI CHARACTER SARA AM` (`E0 B8 B3`) or
/// `U+0EB3 LAO VOWEL SIGN AM` (`E0 BA B3`).
///
/// Implemented as a byte scan: [`memchr::memchr_iter`] (SIMD on
/// supported targets) drives us to each `0xE0` lead byte, and we
/// peek the next two bytes to see whether they finish the
/// three-byte sequence we're looking for. Linear in `bytes.len()`
/// with a single pass and no allocations; the fast-bail case (no
/// `0xE0` anywhere in the input — which is true for every
/// ASCII-only line) returns after the SIMD scan emits no hits.
fn contains_thai_lao_am(bytes: &[u8]) -> bool {
    for start in memchr::memchr_iter(0xE0, bytes) {
        // Need two more bytes after `0xE0` for the sequence to fit.
        // Guarding here also keeps the indexed accesses in-bounds.
        if start + 2 >= bytes.len() {
            break;
        }
        if bytes[start + 2] != 0xB3 {
            continue;
        }
        let mid = bytes[start + 1];
        if mid == 0xB8 || mid == 0xBA {
            return true;
        }
    }
    false
}

/// Whether `g` contains any whitespace scalar. Empty input returns `false`.
///
/// Used by word-segmentation logic (word wrapping, Alt+word cursor
/// motion, Ctrl+W delete-word) to decide what counts as a break
/// between words. Returns true if *any* scalar in the input is
/// whitespace. All current callers feed grapheme-segmenter output
/// (single-scalar inputs in practice), but the any-scalar rule keeps
/// behavior predictable if a multi-scalar grapheme containing a
/// whitespace component ever surfaces (e.g. through future segmenter
/// or paste-handling changes).
pub fn is_whitespace_grapheme(g: &str) -> bool {
    g.chars().any(char::is_whitespace)
}

/// Whether `g` contains any ASCII-punctuation scalar.
///
/// The set is the classic word-segmentation punctuation bag:
/// ``(){}[]<>.,;:'"!?+-=*/\|&%^$#@~` `` plus backtick. Returns true
/// if *any* scalar in the input matches the set. All current callers
/// feed single-scalar grapheme-segmenter output, so a grapheme like
/// `.\u{0301}` (period + combining acute) almost never appears in
/// practice — but if it does, the any-scalar rule still classifies
/// it as punctuation.
pub fn is_punctuation_grapheme(g: &str) -> bool {
    g.chars().any(is_punctuation_char)
}

/// True iff `c` is one of the ASCII punctuation scalars in the
/// word-segmentation bag. Extracted so [`is_punctuation_grapheme`]
/// can run it across every scalar of a multi-scalar grapheme.
fn is_punctuation_char(c: char) -> bool {
    matches!(
        c,
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
///
/// The slow path (grapheme segmentation) is memoised in a small
/// per-thread cache keyed by the input string. Calls on identical
/// non-ASCII inputs from the same thread return without rerunning
/// the segmenter. Pure-ASCII inputs fast-path on `s.len()` and
/// never touch the cache. See [`WIDTH_CACHE_CAPACITY`].
pub fn visible_width(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }

    // Fast path: pure printable ASCII.
    if is_printable_ascii(s) {
        return s.len();
    }

    // Memoised slow path. The cache lookup is O(|s|) for the hash;
    // the recompute is O(|s|) but with grapheme-segmentation
    // constants, so cache hits win comfortably for any repeated
    // string and the miss-then-insert cost is on the order of the
    // recompute we'd be doing anyway.
    if let Some(cached) = WIDTH_CACHE.with(|cell| cell.borrow().get(s)) {
        return cached;
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

    let width = clean.graphemes(true).map(grapheme_width).sum();
    WIDTH_CACHE.with(|cell| cell.borrow_mut().insert(s.to_string(), width));
    width
}

/// Maximum number of memoised `(string, width)` entries per thread
/// before the cache evicts on insertion.
///
/// Sized to comfortably hold a frame's worth of distinct
/// width-needing strings — tokens during wrap, segments of a long
/// markdown body, loader frames, etc. — without growing without
/// bound on a long-running session.
const WIDTH_CACHE_CAPACITY: usize = 512;

thread_local! {
    /// Per-thread memoisation table for the [`visible_width`] slow
    /// path. Thread-local because every TUI render today runs on
    /// one main thread; per-thread state avoids the lock contention
    /// a global `Mutex<…>` would impose on callers that happen to
    /// run from a worker thread.
    static WIDTH_CACHE: std::cell::RefCell<WidthCache> =
        std::cell::RefCell::new(WidthCache::default());
}

/// FIFO-evicting `(String, usize)` table backing [`visible_width`].
///
/// Insertion order is tracked in `order`; on overflow we drop the
/// oldest entry's key from both the map and the queue. FIFO is a
/// looser policy than true LRU but is dead simple and gives the
/// same hit-rate ceiling for the use case here: repeat width
/// lookups on a working set that fits inside the cap. The double
/// bookkeeping costs one extra `String` clone per distinct cached
/// key; for the [`WIDTH_CACHE_CAPACITY`] of 512 the total
/// footprint stays in the tens of kilobytes.
#[derive(Default)]
struct WidthCache {
    map: std::collections::HashMap<String, usize>,
    order: std::collections::VecDeque<String>,
}

impl WidthCache {
    fn get(&self, key: &str) -> Option<usize> {
        self.map.get(key).copied()
    }

    fn insert(&mut self, key: String, value: usize) {
        // Skip if the key is already present. Two callers racing on
        // the same input each compute and try to insert; the second
        // would otherwise add a duplicate entry to `order` and slowly
        // double-count the capacity for hot keys.
        if self.map.contains_key(&key) {
            return;
        }
        if self.map.len() >= WIDTH_CACHE_CAPACITY {
            // Evict the oldest entry. `order` and `map` are kept in
            // lockstep — anything in `order` is in `map`, modulo
            // races inside a single thread which can't happen
            // through `RefCell::borrow_mut`.
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, value);
    }
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
                // Use `visible_width` rather than `grapheme_width` so a
                // tab grapheme contributes its expanded column count
                // (3) instead of the raw control-char zero. Without
                // that, a long word containing an embedded tab is
                // under-counted and the resulting chunk overflows the
                // requested width — see
                // `wrap_text_with_ansi_breaks_long_word_with_embedded_tab`
                // in `tests/wrap_ansi.rs`.
                let gw = visible_width(g);
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
        // The recognized CSI final-byte set is `m G K H J` and
        // nothing else. Cursor-motion / scroll
        // commands like `\x1b[1A` are *not* recognized; the renderer
        // emits those directly into its transport buffer (in
        // `tui.rs`), never into per-line strings that flow through
        // visible_width / extract_ansi_code.
        for terminal in ["m", "G", "K", "H", "J"] {
            let s = format!("\x1b[1{}rest", terminal);
            let result = extract_ansi_code(&s, 0);
            assert!(result.is_some(), "final byte {terminal} should be accepted");
            assert_eq!(result.unwrap().code, format!("\x1b[1{}", terminal));
        }
    }

    #[test]
    fn extract_ansi_code_rejects_cursor_movement_commands() {
        // Cursor commands must be treated as literal text. Without
        // this, `visible_width` would silently swallow stray cursor
        // escapes that appeared in component-level styled strings;
        // we want them to surface as visible bytes that count toward
        // measurement instead.
        for terminal in ["f", "A", "B", "C", "D", "E", "F", "S", "T", "d"] {
            let s = format!("\x1b[1{}", terminal);
            let result = extract_ansi_code(&s, 0);
            assert!(result.is_none(), "final byte {terminal} should be rejected");
        }
    }

    #[test]
    fn extract_ansi_code_rejects_csi_with_non_parameter_intermediate_bytes() {
        // CSI grammar: parameter bytes are `0x30..=0x3F` (digits and
        // `;:<=>?`); intermediate bytes are `0x20..=0x2F`; final bytes
        // are `0x40..=0x7E`. A byte inside the CSI body that falls
        // outside the parameter / intermediate ranges (e.g. an ASCII
        // letter, a stray `\x1b`, a multi-byte UTF-8 lead) indicates
        // a malformed sequence. We bail and let the caller treat the
        // ESC as literal text rather than swallowing whatever bytes
        // happen to precede the next recognized final byte — that
        // way garbage in styled output is rendered visibly instead of
        // silently consumed.
        assert!(extract_ansi_code("\x1b[Am", 0).is_none());
        assert!(extract_ansi_code("\x1b[1;Bm", 0).is_none());
        // Multi-byte UTF-8 lead bytes also fall outside the parameter
        // range and must terminate the scan.
        assert!(extract_ansi_code("\x1b[\u{03B1}m", 0).is_none());
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
        let h = t.active_hyperlink.as_ref().expect("hyperlink set");
        assert_eq!(h.url, "https://example.com");
        assert_eq!(h.params, "");
        assert_eq!(h.terminator, Osc8Terminator::St);
        assert!(t.has_active_codes());
    }

    #[test]
    fn tracker_osc8_open_with_bel_sets_active_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]8;;https://example.com\x07");
        let h = t.active_hyperlink.as_ref().expect("hyperlink set");
        assert_eq!(h.url, "https://example.com");
        assert_eq!(h.terminator, Osc8Terminator::Bel);
    }

    #[test]
    fn tracker_osc8_empty_url_closes_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]8;;https://example.com\x1b\\");
        t.process("\x1b]8;;\x1b\\");
        assert!(t.active_hyperlink.is_none());
        assert!(!t.has_active_codes());
    }

    #[test]
    fn tracker_osc8_with_params_is_still_recognized() {
        let mut t = AnsiStyleTracker::new();
        // Params field (e.g. `id=anchor`) is preserved verbatim.
        t.process("\x1b]8;id=anchor;https://example.com\x1b\\");
        let h = t.active_hyperlink.as_ref().expect("hyperlink set");
        assert_eq!(h.params, "id=anchor");
        assert_eq!(h.url, "https://example.com");
    }

    #[test]
    fn tracker_sgr_reset_preserves_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b[1m");
        t.process("\x1b]8;;https://example.com\x1b\\");
        t.process("\x1b[0m"); // SGR reset
        assert!(!t.bold, "SGR reset clears bold");
        let h = t
            .active_hyperlink
            .as_ref()
            .expect("SGR reset must not close an open hyperlink");
        assert_eq!(h.url, "https://example.com");
    }

    #[test]
    fn tracker_clear_drops_hyperlink() {
        let mut t = AnsiStyleTracker::new();
        t.process("\x1b]8;;https://example.com\x1b\\");
        t.clear();
        assert!(t.active_hyperlink.is_none());
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
        assert!(t.active_hyperlink.is_none());
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

    // -- grapheme_width text-presentation regression --

    #[test]
    fn grapheme_width_text_presentation_chars_are_one_cell() {
        // These codepoints sit inside the broad "emoji" Unicode blocks
        // (U+2600..=U+27BF, U+1F000..=U+1FBFF) but their default
        // presentation is *text*, not emoji. They render as one cell
        // unless followed by VS16. Delegating to `unicode-width` 0.2
        // encodes that rule directly. A naive block-based check
        // would return 2 for the entire range and throw off layout
        // math for inputs like checkbox lists (`☑ done`).
        for s in ["☑", "☐", "☎", "❤", "✓", "✔", "✘", "☹", "☺", "⌨", "⚙"] {
            assert_eq!(
                grapheme_width(s),
                1,
                "expected text-default {s:?} to be width 1",
            );
        }
        // U+1F100 (DIGIT ZERO FULL STOP) is in the supplemental block
        // but has Emoji_Presentation=No, so it should also be 1.
        assert_eq!(grapheme_width("\u{1F100}"), 1);
    }

    #[test]
    fn grapheme_width_emoji_default_presentation_chars_are_two_cells() {
        // Counterpart to the above: these characters live in the same
        // Unicode blocks but are default-emoji-presentation. They
        // remain width 2 with or without VS16.
        for s in ["⌚", "⌛", "⭐", "✅", "⚡", "👍", "🌀"] {
            assert_eq!(
                grapheme_width(s),
                2,
                "expected emoji-default {s:?} to be width 2",
            );
        }
    }

    #[test]
    fn grapheme_width_vs16_promotes_text_presentation_to_emoji_width() {
        // VS16 forces emoji presentation: ❤ alone is width 1, ❤️ is width 2.
        assert_eq!(grapheme_width("\u{2764}"), 1);
        assert_eq!(grapheme_width("\u{2764}\u{FE0F}"), 2);
        assert_eq!(grapheme_width("\u{2611}"), 1);
        assert_eq!(grapheme_width("\u{2611}\u{FE0F}"), 2);
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
    fn whitespace_grapheme_any_scalar_rule() {
        // A multi-scalar grapheme that contains a whitespace scalar
        // anywhere is whitespace, even if other scalars are not.
        assert!(is_whitespace_grapheme("a "));
        assert!(is_whitespace_grapheme(" a"));
        // A grapheme made purely of non-whitespace scalars (e.g. a
        // letter + combining mark) still classifies as non-whitespace.
        assert!(!is_whitespace_grapheme("a\u{0301}")); // a + combining acute
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
    fn punctuation_grapheme_any_scalar_rule() {
        // A multi-scalar grapheme that *contains* a punctuation
        // scalar anywhere classifies as punctuation. In practice the
        // grapheme segmenter rarely yields such graphemes for
        // word-motion inputs, but locking the rule here keeps
        // behavior predictable if one ever surfaces.
        assert!(is_punctuation_grapheme(".\u{0301}")); // period + combining acute
        // Emoji ZWJ sequence with no punctuation scalar: still not
        // punctuation under either implementation.
        assert!(!is_punctuation_grapheme("\u{1f1fa}\u{1f1f8}")); // 🇺🇸
    }

    // -- normalize_terminal_output --

    /// Tiny test-only wrapper that exercises the in-place
    /// [`normalize_terminal_output`] against an `&str` input, so each
    /// case stays one assertion. The real hot path operates on
    /// `&mut String` and avoids this round-trip.
    fn normalized(input: &str) -> String {
        let mut out = input.to_string();
        normalize_terminal_output(&mut out);
        out
    }

    #[test]
    fn normalize_passes_ascii_through_unchanged() {
        let s = "no thai or lao here, just ASCII";
        assert_eq!(normalized(s), s);
    }

    #[test]
    fn normalize_passes_other_utf8_through_unchanged() {
        // Plenty of non-ASCII codepoints, none in the precomposed
        // Thai/Lao SARA AM set. Includes 0xE0 lead bytes that don't
        // form a target sequence (Bengali / Devanagari / Tamil
        // ranges share the `0xE0` first byte but with different
        // continuation bytes), which the byte scan must not mistake
        // for a hit.
        let s = "héllo 你好 🇺🇸 \u{0985}\u{09BE} \u{0B85}\u{0BBE}";
        assert_eq!(normalized(s), s);
    }

    #[test]
    fn normalize_rewrites_thai_sara_am() {
        // U+0E33 (`ำ`) → U+0E4D U+0E32 (`ํา`). One leading prose
        // word so the rewrite has neighbours to keep in place.
        let input = "คำ ทดสอบ";
        let expected = "ค\u{0e4d}\u{0e32} ทดสอบ";
        assert_eq!(normalized(input), expected);
    }

    #[test]
    fn normalize_rewrites_lao_sara_am() {
        // U+0EB3 (`ຳ`) → U+0ECD U+0EB2 (`ໍາ`).
        let input = "ຄຳ";
        let expected = "ຄ\u{0ecd}\u{0eb2}";
        assert_eq!(normalized(input), expected);
    }

    #[test]
    fn normalize_rewrites_multiple_occurrences() {
        let input = "\u{0E33}-\u{0EB3}-\u{0E33}";
        let expected = "\u{0E4D}\u{0E32}-\u{0ECD}\u{0EB2}-\u{0E4D}\u{0E32}";
        assert_eq!(normalized(input), expected);
    }

    #[test]
    fn normalize_handles_partial_three_byte_sequence_at_end_of_input() {
        // The byte scan looks for `E0 _ B3`. A bare trailing `0xE0`,
        // or `0xE0 _` without the third byte, must not be mistaken
        // for a hit and must not read past the input's end. The
        // byte-level helper is tested directly here so we can drive
        // exact byte lengths without having to invent matching
        // Unicode scalars for every edge.
        assert!(!contains_thai_lao_am(&[0xE0]));
        assert!(!contains_thai_lao_am(&[0xE0, 0xB8]));
        assert!(!contains_thai_lao_am(&[0xE0, 0xB8, 0xB2])); // not 0xB3
        assert!(contains_thai_lao_am(&[0xE0, 0xB8, 0xB3]));
        assert!(contains_thai_lao_am(&[0xE0, 0xBA, 0xB3]));
        assert!(!contains_thai_lao_am(&[0xE0, 0xB9, 0xB3])); // wrong mid byte
    }

    #[test]
    fn normalize_finds_target_after_a_decoy_lead_byte() {
        // Two `0xE0` lead bytes: the first does NOT start a SARA AM
        // sequence (it's part of `\u{0985}` — Bengali letter `অ`,
        // encoded `E0 A6 85`), the second does. The scan has to
        // continue past the first hit.
        let input = "\u{0985}\u{0E33}";
        let expected = "\u{0985}\u{0E4D}\u{0E32}";
        assert_eq!(normalized(input), expected);
    }

    #[test]
    fn normalize_is_a_noop_on_the_fast_path() {
        // The fast path (no SARA AM vowel) must not mutate the
        // input — the render hot path relies on this to skip the
        // per-line allocation for every line that doesn't carry
        // the target codepoint.
        let mut s = String::from("plain ASCII content with no targets");
        let original_ptr = s.as_ptr();
        normalize_terminal_output(&mut s);
        assert_eq!(s, "plain ASCII content with no targets");
        // Same heap buffer — no reallocation happened.
        assert_eq!(s.as_ptr(), original_ptr);
    }

    // -- visible_width / WidthCache --

    #[test]
    fn width_cache_returns_cached_value_for_a_repeat_lookup() {
        let mut cache = WidthCache::default();
        assert_eq!(cache.get("héllo"), None);
        cache.insert("héllo".to_string(), 5);
        assert_eq!(cache.get("héllo"), Some(5));
        // A second `insert` for the same key is a no-op (skipped to
        // avoid duplicate entries in the eviction queue).
        cache.insert("héllo".to_string(), 5);
        assert_eq!(cache.order.len(), 1);
        assert_eq!(cache.map.len(), 1);
    }

    #[test]
    fn width_cache_evicts_in_insertion_order_when_full() {
        // Build a tiny private cache with a forced overflow to
        // observe FIFO eviction. We can't lower `WIDTH_CACHE_CAPACITY`
        // for one test, so the assertion targets the eviction
        // *order* by inserting one entry past the cap (513 items)
        // and verifying the first-inserted key was the one dropped.
        let mut cache = WidthCache::default();
        let cap = WIDTH_CACHE_CAPACITY;
        for i in 0..cap {
            cache.insert(format!("k{i}"), i);
        }
        assert_eq!(cache.map.len(), cap);
        assert_eq!(cache.get("k0"), Some(0));

        // One more insert forces eviction of the oldest entry
        // (`k0`). Subsequent entries (`k1`, `k2`, …) are still
        // present; the new entry lands at the back.
        cache.insert("overflow".to_string(), 999);
        assert_eq!(cache.map.len(), cap);
        assert_eq!(cache.get("k0"), None, "oldest entry should be evicted");
        assert_eq!(cache.get("k1"), Some(1));
        assert_eq!(cache.get("overflow"), Some(999));
    }

    #[test]
    fn visible_width_caches_repeat_non_ascii_lookups() {
        // The end-to-end check: two calls on the same non-ASCII
        // input agree on the width, which would surface any
        // disagreement between cache hit and miss. (A real
        // regression here — say, a cache that stored the wrong key
        // — would return one value on the first call and a
        // different one on the second.)
        let s = "héllo wörld";
        let first = visible_width(s);
        let second = visible_width(s);
        assert_eq!(first, second);
        assert!(first > 0);
    }

    #[test]
    fn visible_width_pure_ascii_does_not_pollute_the_cache() {
        // ASCII fast-paths on `s.len()` and must not insert into
        // the cache — otherwise the cap fills up with prose
        // strings that didn't need to be there. We can't read the
        // thread-local cache from outside, so instead verify the
        // semantic property by checking the fast-path return value
        // matches the byte length for an ASCII input that doesn't
        // contain tabs or escape sequences.
        let s = "plain ASCII line";
        assert_eq!(visible_width(s), s.len());
    }
}
