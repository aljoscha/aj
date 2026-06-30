//! The input escape-sequence parser decoding terminal bytes into events.
//!
//! The parser is a lookahead, recursive-descent dispatcher, not a running byte
//! state machine. [`Parser::parse`] inspects the first byte (and, for an escape,
//! the second) and hands off to a sub-parser for the rest of the sequence. Each
//! sub-parser returns a [`ParseResult`] carrying the decoded event (if any) and
//! the number of bytes it consumed.
//!
//! The `n` field drives the caller's resync:
//!
//! - `n == 0` means the buffer holds an incomplete sequence. The caller should
//!   read more bytes and retry from the same offset.
//! - `n > 0` with `event == None` means a complete but uninteresting sequence
//!   was consumed. The caller skips `n` bytes and parses again.
//! - `n > 0` with `event == Some(_)` is a decoded event spanning `n` bytes.

use base64::Engine as _;

use crate::Winsize;
use crate::cell::{Color, ColorSpecError, Kind, Report, Scheme};
use crate::event::Event;
use crate::key::{Key, Modifiers};
use crate::mouse::{Button, Mouse, Type};
use compact_str::CompactString;

/// SGR/X10 mouse report bit masks. `button_mask` is `u16` so the `leave` bit
/// (which only exists in the extended SGR-pixels report) does not collide with
/// the legacy fields.
mod mouse_bits {
    pub const MOTION: u16 = 0b0010_0000;
    pub const BUTTONS: u16 = 0b1100_0011;
    pub const SHIFT: u16 = 0b0000_0100;
    pub const ALT: u16 = 0b0000_1000;
    pub const CTRL: u16 = 0b0001_0000;
    pub const LEAVE: u16 = 0b1_0000_0000;
}

/// The decoded event and the number of input bytes it spans.
///
/// See the module docs for the meaning of `n`. Named `ParseResult` rather than
/// `Result` to avoid shadowing [`std::result::Result`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseResult {
    pub event: Option<Event>,
    pub n: usize,
}

/// Errors the parser surfaces. Most malformed input degrades to a skip result
/// (`event: None, n > 0`) rather than an error. These variants cover only the
/// cases upstream propagates: an undecodable leading grapheme, a bad OSC color
/// spec, and a bad OSC 52 base64 payload.
///
/// Numeric parameter parse failures (modifier masks, key numbers, capability
/// codes) are intentionally not errors here. They degrade to the skip result,
/// matching the parser's behavior. A malformed hex color channel still surfaces
/// through [`ColorSpecError::ParseInt`] inside [`ParseError::ColorSpec`].
#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("input did not start with a valid UTF-8 codepoint")]
    InvalidUtf8,
    #[error(transparent)]
    ColorSpec(#[from] ColorSpecError),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
}

/// Decodes terminal input bytes into [`Event`]s.
///
/// Unlike upstream, which keeps a 128-byte scratch buffer to re-encode
/// text-as-codepoints, our `Key::text` is owned, so we build text into owned
/// buffers directly and the parser carries no state. `parse` still takes
/// `&mut self` to match the upstream method shape and to leave room for future
/// state.
#[derive(Debug, Default, Clone, Copy)]
pub struct Parser;

impl Parser {
    pub fn new() -> Self {
        Self
    }

    /// Parses the first event from `input`.
    ///
    /// `input` must be non-empty. See the module docs for how to act on the
    /// returned `n`.
    pub fn parse(&mut self, input: &[u8]) -> Result<ParseResult, ParseError> {
        debug_assert!(!input.is_empty());

        // We gate on len > 1 so a lone 0x1b is detected as the escape key
        // rather than the start of a sequence.
        if input[0] == 0x1b && input.len() > 1 {
            match input[1] {
                0x4F => Ok(parse_ss3(input)),     // SS3
                0x50 => Ok(skip_until_st(input)), // DCS
                0x58 => Ok(skip_until_st(input)), // SOS
                0x5B => Ok(parse_csi(input)),     // CSI
                0x5D => parse_osc(input),         // OSC
                0x5E => Ok(skip_until_st(input)), // PM
                0x5F => Ok(parse_apc(input)),     // APC
                other => {
                    // Anything else after the escape is an "alt + <char>".
                    let key = Key {
                        codepoint: u32::from(other),
                        mods: Modifiers::ALT,
                        ..Default::default()
                    };
                    Ok(ParseResult {
                        event: Some(Event::KeyPress(key)),
                        n: 2,
                    })
                }
            }
        } else {
            parse_ground(input)
        }
    }
}

/// Parses a ground-state byte: a C0 control mapped to its Ctrl key, or the
/// first grapheme of a UTF-8 run.
fn parse_ground(input: &[u8]) -> Result<ParseResult, ParseError> {
    debug_assert!(!input.is_empty());

    let b = input[0];
    let mut n: usize = 1;
    // Bytes below 0x20 are Ctrl+<c> presses, mapped to a lowercase letter where
    // we can. The rest decode as text.
    let key = match b {
        0x00 => Key {
            codepoint: u32::from(b'@'),
            mods: Modifiers::CTRL,
            ..Default::default()
        },
        0x08 => Key {
            codepoint: Key::BACKSPACE,
            ..Default::default()
        },
        0x09 => Key {
            codepoint: Key::TAB,
            ..Default::default()
        },
        0x0A => Key {
            codepoint: u32::from(b'j'),
            mods: Modifiers::CTRL,
            ..Default::default()
        },
        0x0D => Key {
            codepoint: Key::ENTER,
            ..Default::default()
        },
        0x01..=0x07 | 0x0B..=0x0C | 0x0E..=0x1A => Key {
            codepoint: u32::from(b) + 0x60,
            mods: Modifiers::CTRL,
            ..Default::default()
        },
        0x1B => {
            // parse only routes a lone 0x1b here; a sequence-leading escape has
            // len > 1 and is handled in `parse`.
            debug_assert!(input.len() == 1);
            Key {
                codepoint: Key::ESCAPE,
                ..Default::default()
            }
        }
        0x7F => Key {
            codepoint: Key::BACKSPACE,
            ..Default::default()
        },
        _ => {
            // Decode over the longest valid UTF-8 prefix so trailing partial or
            // invalid bytes do not prevent recognizing the leading grapheme.
            let valid = match std::str::from_utf8(input) {
                Ok(s) => s,
                Err(e) => {
                    let up_to = e.valid_up_to();
                    if up_to == 0 {
                        return Err(ParseError::InvalidUtf8);
                    }
                    std::str::from_utf8(&input[..up_to]).expect("prefix is valid utf-8")
                }
            };
            let Some(grapheme) = crate::unicode::grapheme_iterator(valid).next() else {
                return Err(ParseError::InvalidUtf8);
            };
            let cluster = grapheme.bytes(valid);
            n = grapheme.len;
            // A grapheme spanning more than one codepoint has no single-scalar
            // representation, so we flag it and carry the cluster as text.
            let codepoint = if cluster.chars().count() > 1 {
                Key::MULTICODEPOINT
            } else {
                u32::from(cluster.chars().next().expect("non-empty cluster"))
            };
            Key {
                codepoint,
                text: Some(CompactString::from(cluster)),
                ..Default::default()
            }
        }
    };

    Ok(ParseResult {
        event: Some(Event::KeyPress(key)),
        n,
    })
}

/// Parses an `SS3` (`ESC O`) application-mode arrow/function key.
fn parse_ss3(input: &[u8]) -> ParseResult {
    if input.len() < 3 {
        return ParseResult { event: None, n: 0 };
    }
    let codepoint = match input[2] {
        0x1B => {
            return ParseResult { event: None, n: 2 };
        }
        b'A' => Key::UP,
        b'B' => Key::DOWN,
        b'C' => Key::RIGHT,
        b'D' => Key::LEFT,
        b'E' => Key::KP_BEGIN,
        b'F' => Key::END,
        b'H' => Key::HOME,
        b'P' => Key::F1,
        b'Q' => Key::F2,
        b'R' => Key::F3,
        b'S' => Key::F4,
        _ => {
            return ParseResult { event: None, n: 3 };
        }
    };
    ParseResult {
        event: Some(Event::KeyPress(Key {
            codepoint,
            ..Default::default()
        })),
        n: 3,
    }
}

/// Parses an `APC` (`ESC _`) sequence, recognizing only the kitty-graphics
/// response.
fn parse_apc(input: &[u8]) -> ParseResult {
    if input.len() < 3 {
        return ParseResult { event: None, n: 0 };
    }
    let Some(end) = index_of_scalar_pos(input, 2, 0x1b) else {
        return ParseResult { event: None, n: 0 };
    };
    // NOTE: upstream slices `input[0..end+2]` without the length guard that
    // skipUntilST has, so n can exceed input.len() when the ST's trailing byte
    // has not arrived. We reproduce the arithmetic; computing the length cannot
    // index out of bounds.
    let seq_len = end + 2;
    match input[2] {
        b'G' => ParseResult {
            event: Some(Event::CapKittyGraphics),
            n: seq_len,
        },
        _ => ParseResult {
            event: None,
            n: seq_len,
        },
    }
}

/// Skips a string sequence up to and including its `ST` (`ESC \`).
///
/// Returns an incomplete result (`n == 0`) until the `ST` and its trailing byte
/// are present. Used for `DCS`, `SOS`, `PM`, and as the `ST` finder for `OSC`.
fn skip_until_st(input: &[u8]) -> ParseResult {
    if input.len() < 3 {
        return ParseResult { event: None, n: 0 };
    }
    let Some(end) = index_of_scalar_pos(input, 2, 0x1b) else {
        return ParseResult { event: None, n: 0 };
    };
    if input.len() < end + 2 {
        return ParseResult { event: None, n: 0 };
    }
    ParseResult {
        event: None,
        n: end + 2,
    }
}

/// Parses an `OSC` (`ESC ]`) sequence: color reports (OSC 4/10/11/12) and the
/// OSC 52 clipboard paste.
fn parse_osc(input: &[u8]) -> Result<ParseResult, ParseError> {
    if input.len() < 3 {
        return Ok(ParseResult { event: None, n: 0 });
    }

    // The sequence may end with an ST (ESC \) or a bare BEL. `end` is one past
    // the final terminator byte, so `input[0..end]` is the whole sequence.
    let mut bel_terminated = false;
    let end = {
        let st = skip_until_st(input);
        if st.n > 0 {
            st.n
        } else {
            let Some(bel) = index_of_scalar_pos(input, 2, 0x07) else {
                return Ok(ParseResult { event: None, n: 0 });
            };
            bel_terminated = true;
            bel + 1
        }
    };
    let seq_len = end;
    let skip = || ParseResult {
        event: None,
        n: seq_len,
    };

    let Some(semicolon_idx) = index_of_scalar_pos(input, 2, b';') else {
        return Ok(skip());
    };
    let Some(ps) = parse_param::<u8>(&input[2..semicolon_idx], None) else {
        return Ok(skip());
    };

    // Strip the terminator from the payload: one byte for BEL, two for ST.
    let content_end = if bel_terminated {
        seq_len - 1
    } else {
        seq_len - 2
    };

    match ps {
        4 => {
            let Some(color_idx_delim) = index_of_scalar_pos(input, semicolon_idx + 1, b';') else {
                return Ok(skip());
            };
            let Some(ps_idx) = parse_param::<u8>(&input[semicolon_idx + 1..color_idx_delim], None)
            else {
                return Ok(skip());
            };
            let Some(spec) = input.get(color_idx_delim + 1..content_end) else {
                return Ok(skip());
            };
            let value = rgb_from_spec_bytes(spec)?;
            Ok(ParseResult {
                event: Some(Event::ColorReport(Report {
                    kind: Kind::Index(ps_idx),
                    value,
                })),
                n: seq_len,
            })
        }
        10 | 11 | 12 => {
            let Some(spec) = input.get(semicolon_idx + 1..content_end) else {
                return Ok(skip());
            };
            let value = rgb_from_spec_bytes(spec)?;
            let kind = match ps {
                10 => Kind::Fg,
                11 => Kind::Bg,
                _ => Kind::Cursor,
            };
            Ok(ParseResult {
                event: Some(Event::ColorReport(Report { kind, value })),
                n: seq_len,
            })
        }
        52 => {
            // OSC 52 requires the 'c' (clipboard) selector after the semicolon,
            // then a semicolon, then the base64 payload.
            if input.get(semicolon_idx + 1) != Some(&b'c') {
                return Ok(skip());
            }
            let Some(payload) = input.get(semicolon_idx + 3..content_end) else {
                return Ok(skip());
            };
            let decoded = base64::engine::general_purpose::STANDARD.decode(payload)?;
            // Event::Paste is an owned String. Clipboard contents are normally
            // text, so we lossily decode rather than failing the whole parse on
            // a stray non-UTF-8 byte.
            let text = String::from_utf8_lossy(&decoded).into_owned();
            Ok(ParseResult {
                event: Some(Event::Paste(text)),
                n: seq_len,
            })
        }
        _ => Ok(skip()),
    }
}

/// Parses a `CSI` (`ESC [`) sequence. This is the dispatch on the final byte:
/// legacy cursor/function keys, kitty keyboard, mouse, focus, and the various
/// capability probes.
fn parse_csi(input: &[u8]) -> ParseResult {
    if input.len() < 3 {
        return ParseResult { event: None, n: 0 };
    }
    // Scan from past the '[' for the final byte (0x40..=0xFF). Everything before
    // it is parameter/intermediate bytes.
    let mut final_idx = None;
    for (i, &b) in input.iter().enumerate().skip(2) {
        if (0x40..=0xFF).contains(&b) {
            final_idx = Some(i);
            break;
        }
    }
    let Some(final_idx) = final_idx else {
        return ParseResult { event: None, n: 0 };
    };
    let seq_len = final_idx + 1;
    let sequence = &input[..seq_len];
    let final_byte = sequence[seq_len - 1];
    let skip = || ParseResult {
        event: None,
        n: seq_len,
    };

    match final_byte {
        // Legacy cursor/function keys:
        //   CSI {ABCDEFHPQS}
        //   CSI 1 ; modifier:event_type {ABCDEFHPQS}
        b'A' | b'B' | b'C' | b'D' | b'E' | b'F' | b'H' | b'P' | b'Q' | b'R' | b'S' => {
            let codepoint = match final_byte {
                b'A' => Key::UP,
                b'B' => Key::DOWN,
                b'C' => Key::RIGHT,
                b'D' => Key::LEFT,
                b'E' => Key::KP_BEGIN,
                b'F' => Key::END,
                b'H' => Key::HOME,
                b'P' => Key::F1,
                b'Q' => Key::F2,
                b'R' => Key::F3,
                b'S' => Key::F4,
                _ => return skip(),
            };
            let mut fields = sequence[2..seq_len - 1].split(|&b| b == b';');
            fields.next(); // skip the leading (always-present) field
            let mut key = Key {
                codepoint,
                ..Default::default()
            };
            let Some(is_release) = parse_mods_and_text(&mut fields, &mut key) else {
                return skip();
            };
            press_or_release(key, is_release, seq_len)
        }
        // CSI Z is shift+tab.
        b'Z' => ParseResult {
            event: Some(Event::KeyPress(Key {
                codepoint: Key::TAB,
                mods: Modifiers::SHIFT,
                ..Default::default()
            })),
            n: seq_len,
        },
        // Numbered keys:
        //   CSI number ~
        //   CSI number ; modifier:event_type ; text_as_codepoint ~
        b'~' => {
            let mut fields = sequence[2..seq_len - 1].split(|&b| b == b';');
            let number_buf = fields.next().expect("split yields at least one field");
            let Some(number) = parse_param::<u16>(number_buf, None) else {
                return skip();
            };
            let codepoint = match number {
                2 => Key::INSERT,
                3 => Key::DELETE,
                5 => Key::PAGE_UP,
                6 => Key::PAGE_DOWN,
                7 => Key::HOME,
                8 => Key::END,
                11 => Key::F1,
                12 => Key::F2,
                13 => Key::F3,
                14 => Key::F4,
                15 => Key::F5,
                17 => Key::F6,
                18 => Key::F7,
                19 => Key::F8,
                20 => Key::F9,
                21 => Key::F10,
                23 => Key::F11,
                24 => Key::F12,
                200 => {
                    return ParseResult {
                        event: Some(Event::PasteStart),
                        n: seq_len,
                    };
                }
                201 => {
                    return ParseResult {
                        event: Some(Event::PasteEnd),
                        n: seq_len,
                    };
                }
                57427 => Key::KP_BEGIN,
                _ => return skip(),
            };
            let mut key = Key {
                codepoint,
                ..Default::default()
            };
            let Some(is_release) = parse_mods_and_text(&mut fields, &mut key) else {
                return skip();
            };
            press_or_release(key, is_release, seq_len)
        }
        b'I' => ParseResult {
            event: Some(Event::FocusIn),
            n: seq_len,
        },
        b'O' => ParseResult {
            event: Some(Event::FocusOut),
            n: seq_len,
        },
        b'M' | b'm' => parse_mouse(sequence, input),
        b'c' => {
            // Primary DA: CSI ? Pm c
            debug_assert!(seq_len >= 4); // ESC [ ? c
            if input[2] == b'?' {
                ParseResult {
                    event: Some(Event::CapDa1),
                    n: seq_len,
                }
            } else {
                skip()
            }
        }
        b'n' => {
            // Device Status Report: CSI ? Ps ; Pm n
            debug_assert!(seq_len >= 3);
            if sequence[2] != b'?' {
                return skip();
            }
            let Some(delim_idx) = index_of_scalar_pos(input, 3, b';') else {
                return skip();
            };
            let Some(ps) = parse_param::<u16>(&input[3..delim_idx], None) else {
                return skip();
            };
            if ps != 997 {
                return skip();
            }
            // Color-scheme update notification.
            match sequence.get(delim_idx + 1) {
                Some(b'1') => ParseResult {
                    event: Some(Event::ColorScheme(Scheme::Dark)),
                    n: seq_len,
                },
                Some(b'2') => ParseResult {
                    event: Some(Event::ColorScheme(Scheme::Light)),
                    n: seq_len,
                },
                _ => skip(),
            }
        }
        b't' => {
            // XTWINOPS in-band resize: CSI 48 ; rows ; cols ; ypix ; xpix t
            let mut fields = sequence[2..seq_len - 1].split(|&b| b == b';');
            let ps = fields.next().expect("split yields at least one field");
            if ps != b"48" {
                return skip();
            }
            let Some(height_char) = fields.next() else {
                return skip();
            };
            let Some(width_char) = fields.next() else {
                return skip();
            };
            let height_pix = fields.next().unwrap_or(b"0".as_slice());
            let width_pix = fields.next().unwrap_or(b"0".as_slice());
            let (Some(rows), Some(cols), Some(x_pixel), Some(y_pixel)) = (
                parse_param::<u16>(height_char, None),
                parse_param::<u16>(width_char, None),
                parse_param::<u16>(width_pix, None),
                parse_param::<u16>(height_pix, None),
            ) else {
                return skip();
            };
            ParseResult {
                event: Some(Event::Winsize(Winsize {
                    rows,
                    cols,
                    x_pixel,
                    y_pixel,
                })),
                n: seq_len,
            }
        }
        b'u' => parse_kitty_keyboard(sequence, seq_len),
        b'y' => {
            // DECRPM: CSI ? Ps ; Pm $ y
            let Some(delim_idx) = index_of_scalar_pos(input, 3, b';') else {
                return skip();
            };
            let Some(ps) = parse_param::<u16>(&input[3..delim_idx], None) else {
                return skip();
            };
            let Some(pm) = input
                .get(delim_idx + 1..seq_len - 2)
                .and_then(parse_param_uint::<u8>)
            else {
                return skip();
            };
            let cap = match ps {
                1016 => Event::CapSgrPixels,          // Mouse pixel reporting
                2027 => Event::CapUnicode,            // Unicode core
                2031 => Event::CapColorSchemeUpdates, // Color scheme reporting
                _ => return skip(),
            };
            // pm 0 (not recognized) and 4 (permanently reset) mean unsupported.
            match pm {
                0 | 4 => skip(),
                _ => ParseResult {
                    event: Some(cap),
                    n: seq_len,
                },
            }
        }
        b'q' => {
            // Kitty multi-cursor cap: CSI > ... SP q
            let second_final = sequence[seq_len - 2];
            if second_final != b' ' {
                return skip();
            }
            if sequence[..seq_len - 2].iter().any(u8::is_ascii_digit) {
                ParseResult {
                    event: Some(Event::CapMultiCursor),
                    n: seq_len,
                }
            } else {
                skip()
            }
        }
        _ => skip(),
    }
}

/// Parses the kitty keyboard `CSI u` sequence and applies the shift-synthesis
/// rule. `sequence` is the full CSI sequence including `ESC [` and the `u`.
fn parse_kitty_keyboard(sequence: &[u8], seq_len: usize) -> ParseResult {
    let skip = || ParseResult {
        event: None,
        n: seq_len,
    };

    // CSI ? u is the kitty-keyboard capability probe response.
    if sequence.len() > 2 && sequence[2] == b'?' {
        return ParseResult {
            event: Some(Event::CapKittyKeyboard),
            n: seq_len,
        };
    }

    let mut fields = sequence[2..seq_len - 1].split(|&b| b == b';');
    let mut key = Key::default();

    // Field 1: unicode-key-code : shifted_codepoint : base_layout_codepoint
    {
        let field_buf = fields.next().expect("split yields at least one field");
        let mut params = field_buf.split(|&b| b == b':');
        let codepoint_buf = params.next().expect("split yields at least one field");
        let Some(codepoint) = parse_param::<u32>(codepoint_buf, None) else {
            return skip();
        };
        key.codepoint = codepoint;
        if let Some(shifted_buf) = params.next() {
            key.shifted_codepoint = parse_param::<u32>(shifted_buf, None);
        }
        if let Some(base_buf) = params.next() {
            key.base_layout_codepoint = parse_param::<u32>(base_buf, None);
        }
    }

    let Some(is_release) = parse_mods_and_text(&mut fields, &mut key) else {
        return skip();
    };

    // Shift-synthesis: with disambiguation on, a printable key pressed with only
    // shift can arrive as e.g. `CSI 32 ; 2 u` (shift+space) carrying no text. We
    // synthesize the uppercase text and shifted codepoint the terminal omitted.
    let mod_test = Modifiers::SHIFT | (key.mods & (Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK));
    let printable_byte = if key.codepoint <= u32::from(u8::MAX) {
        u8::try_from(key.codepoint).ok()
    } else {
        None
    };
    if key.text.is_none()
        && key.mods.eql(mod_test)
        && printable_byte.is_some_and(|b| (0x20..=0x7e).contains(&b))
    {
        let upper = printable_byte
            .expect("checked is_some")
            .to_ascii_uppercase();
        let mut text = CompactString::default();
        text.push(char::from(upper));
        key.text = Some(text);
        key.shifted_codepoint = Some(u32::from(upper));
    }

    press_or_release(key, is_release, seq_len)
}

/// Parses the shared `modifier_mask:event_type` (field 2) and
/// `text-as-codepoints` (field 3) fields used by legacy keys, numbered keys,
/// and kitty keys. Advances `fields` past them and mutates `key`.
///
/// Returns the release flag, or `None` when a field is malformed (the caller
/// maps that to the skip result).
fn parse_mods_and_text<'a, I>(fields: &mut I, key: &mut Key) -> Option<bool>
where
    I: Iterator<Item = &'a [u8]>,
{
    let mut is_release = false;

    if let Some(field_buf) = fields.next() {
        let mut params = field_buf.split(|&b| b == b':');
        let modifier_buf = params.next().expect("split yields at least one field");
        let modifier_mask = parse_param::<u8>(modifier_buf, Some(1))?;
        // Kitty encodes modifiers as `mask - 1`; reinterpreting those bits gives
        // our Modifiers directly. Saturating keeps a zero mask at no-modifiers.
        key.mods = Modifiers::from_bits_retain(modifier_mask.saturating_sub(1));
        if let Some(event_type_buf) = params.next() {
            is_release = event_type_buf == b"3";
        }
    }

    if let Some(field_buf) = fields.next() {
        let mut text = CompactString::default();
        for cp_buf in field_buf.split(|&b| b == b':') {
            let cp = parse_param::<u32>(cp_buf, None)?;
            let ch = char::from_u32(cp)?;
            text.push(ch);
        }
        key.text = Some(text);
    }

    Some(is_release)
}

/// Wraps `key` in a press or release event spanning `seq_len` bytes.
fn press_or_release(key: Key, is_release: bool, seq_len: usize) -> ParseResult {
    let event = if is_release {
        Event::KeyRelease(key)
    } else {
        Event::KeyPress(key)
    };
    ParseResult {
        event: Some(event),
        n: seq_len,
    }
}

/// Parses a mouse report, both SGR (`CSI < ... M/m`) and legacy X10
/// (`CSI M` plus three raw bytes). `sequence` is the CSI sequence; `full_input`
/// is the original buffer, needed for the X10 coordinate bytes that follow the
/// final `M`.
fn parse_mouse(sequence: &[u8], full_input: &[u8]) -> ParseResult {
    let skip_n = sequence.len();
    let skip = || ParseResult {
        event: None,
        n: skip_n,
    };

    let (button_mask, px, py, xterm): (u16, i16, i16, bool) =
        if sequence.len() == 3 && sequence[2] == b'M' && full_input.len() >= 6 {
            // X10: button and coordinates are biased by 32 in the three bytes
            // after the `M`.
            (
                u16::from(full_input[3].wrapping_sub(32)),
                i16::from(full_input[4].wrapping_sub(32)),
                i16::from(full_input[5].wrapping_sub(32)),
                true,
            )
        } else if sequence.len() >= 4 && sequence[2] == b'<' {
            let Some(delim1) = index_of_scalar_pos(sequence, 3, b';') else {
                return skip();
            };
            let Some(button_mask) = parse_param::<u16>(&sequence[3..delim1], None) else {
                return skip();
            };
            let Some(delim2) = index_of_scalar_pos(sequence, delim1 + 1, b';') else {
                return skip();
            };
            let Some(px) = parse_param::<i16>(&sequence[delim1 + 1..delim2], Some(1)) else {
                return skip();
            };
            let Some(py) = parse_param::<i16>(&sequence[delim2 + 1..sequence.len() - 1], Some(1))
            else {
                return skip();
            };
            (button_mask, px, py, false)
        } else {
            return skip();
        };

    let n = if xterm { 6 } else { sequence.len() };

    if button_mask & mouse_bits::LEAVE != 0 {
        return ParseResult {
            event: Some(Event::MouseLeave),
            n,
        };
    }

    let masked = u8::try_from(button_mask & mouse_bits::BUTTONS).expect("0xC3 mask fits in u8");
    // NOTE: the 0xC3 mask can still yield codes (192..=195) that map to no
    // button. Upstream reaches for an undefined enum value there; we skip the
    // malformed report instead of risking UB.
    let Ok(button) = Button::try_from(masked) else {
        return ParseResult { event: None, n };
    };

    let motion = button_mask & mouse_bits::MOTION != 0;
    let mut mods = crate::mouse::Modifiers::empty();
    mods.set(
        crate::mouse::Modifiers::SHIFT,
        button_mask & mouse_bits::SHIFT != 0,
    );
    mods.set(
        crate::mouse::Modifiers::ALT,
        button_mask & mouse_bits::ALT != 0,
    );
    mods.set(
        crate::mouse::Modifiers::CTRL,
        button_mask & mouse_bits::CTRL != 0,
    );

    let kind = if motion && button != Button::None {
        Type::Drag
    } else if motion {
        Type::Motion
    } else if xterm {
        if button == Button::None {
            Type::Release
        } else {
            Type::Press
        }
    } else if sequence[sequence.len() - 1] == b'm' {
        Type::Release
    } else {
        Type::Press
    };

    let mouse = Mouse {
        // Coordinates are 1-based on the wire and saturate so negative SGR
        // positions stay representable rather than wrapping.
        col: px.saturating_sub(1),
        row: py.saturating_sub(1),
        xoffset: 0,
        yoffset: 0,
        button,
        mods,
        kind,
    };
    ParseResult {
        event: Some(Event::Mouse(mouse)),
        n,
    }
}

/// Returns the index of the first `needle` at or after `start`, mirroring
/// `std.mem.indexOfScalarPos`.
fn index_of_scalar_pos(haystack: &[u8], start: usize, needle: u8) -> Option<usize> {
    haystack
        .iter()
        .enumerate()
        .skip(start)
        .find(|&(_, &b)| b == needle)
        .map(|(i, _)| i)
}

/// Parses a decimal parameter, returning `default` when the field is empty and
/// `None` on a parse error.
fn parse_param<T: std::str::FromStr>(buf: &[u8], default: Option<T>) -> Option<T> {
    if buf.is_empty() {
        return default;
    }
    std::str::from_utf8(buf).ok()?.parse::<T>().ok()
}

/// `parse_param` with no default, as a named function so it can be passed to
/// combinators like `Option::and_then`.
fn parse_param_uint<T: std::str::FromStr>(buf: &[u8]) -> Option<T> {
    parse_param(buf, None)
}

/// Parses an XParseColor RGB spec from raw bytes into the three channels.
fn rgb_from_spec_bytes(spec: &[u8]) -> Result<[u8; 3], ParseError> {
    let spec = std::str::from_utf8(spec).map_err(|_| ColorSpecError::InvalidSpec)?;
    match Color::rgb_from_spec(spec)? {
        Color::Rgb(rgb) => Ok(rgb),
        _ => unreachable!("rgb_from_spec only produces Color::Rgb"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(input: &[u8]) -> ParseResult {
        Parser::new().parse(input).expect("parse should succeed")
    }

    #[test]
    fn parse_single_xterm_keypress() {
        let result = parse(b"a");
        let expected = Event::KeyPress(Key {
            codepoint: u32::from('a'),
            text: Some("a".into()),
            ..Default::default()
        });
        assert_eq!(result.n, 1);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_single_xterm_keypress_backspace() {
        let result = parse(b"\x08");
        let expected = Event::KeyPress(Key {
            codepoint: Key::BACKSPACE,
            ..Default::default()
        });
        assert_eq!(result.n, 1);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_single_xterm_keypress_with_more_buffer() {
        let result = parse(b"ab");
        let expected = Event::KeyPress(Key {
            codepoint: u32::from('a'),
            text: Some("a".into()),
            ..Default::default()
        });
        assert_eq!(result.n, 1);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_xterm_escape_keypress() {
        let result = parse(b"\x1b");
        let expected = Event::KeyPress(Key {
            codepoint: Key::ESCAPE,
            ..Default::default()
        });
        assert_eq!(result.n, 1);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_xterm_ctrl_a() {
        let result = parse(b"\x01");
        let expected = Event::KeyPress(Key {
            codepoint: u32::from('a'),
            mods: Modifiers::CTRL,
            ..Default::default()
        });
        assert_eq!(result.n, 1);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_xterm_alt_a() {
        let result = parse(b"\x1ba");
        let expected = Event::KeyPress(Key {
            codepoint: u32::from('a'),
            mods: Modifiers::ALT,
            ..Default::default()
        });
        assert_eq!(result.n, 2);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_xterm_key_up() {
        // Normal form.
        let result = parse(b"\x1b[A");
        let expected = Event::KeyPress(Key {
            codepoint: Key::UP,
            ..Default::default()
        });
        assert_eq!(result.n, 3);
        assert_eq!(result.event, Some(expected));

        // Application-keys form.
        let result = parse(b"\x1bOA");
        let expected = Event::KeyPress(Key {
            codepoint: Key::UP,
            ..Default::default()
        });
        assert_eq!(result.n, 3);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_xterm_shift_up() {
        let result = parse(b"\x1b[1;2A");
        let expected = Event::KeyPress(Key {
            codepoint: Key::UP,
            mods: Modifiers::SHIFT,
            ..Default::default()
        });
        assert_eq!(result.n, 6);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_xterm_shift_tab() {
        let result = parse(b"\x1b[Z");
        let expected = Event::KeyPress(Key {
            codepoint: Key::TAB,
            mods: Modifiers::SHIFT,
            ..Default::default()
        });
        assert_eq!(result.n, 3);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_xterm_insert() {
        let result = parse(b"\x1b[2~");
        let expected = Event::KeyPress(Key {
            codepoint: Key::INSERT,
            ..Default::default()
        });
        assert_eq!(result.n, 4);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_paste_start() {
        let result = parse(b"\x1b[200~");
        assert_eq!(result.n, 6);
        assert_eq!(result.event, Some(Event::PasteStart));
    }

    #[test]
    fn parse_paste_end() {
        let result = parse(b"\x1b[201~");
        assert_eq!(result.n, 6);
        assert_eq!(result.event, Some(Event::PasteEnd));
    }

    #[test]
    fn parse_osc52_paste() {
        let result = parse(b"\x1b]52;c;b3NjNTIgcGFzdGU=\x1b\\");
        assert_eq!(result.n, 25);
        match result.event {
            Some(Event::Paste(text)) => assert_eq!(text, "osc52 paste"),
            other => panic!("expected a paste event, got {other:?}"),
        }
    }

    #[test]
    fn parse_focus_in() {
        let result = parse(b"\x1b[I");
        assert_eq!(result.n, 3);
        assert_eq!(result.event, Some(Event::FocusIn));
    }

    #[test]
    fn parse_focus_out() {
        let result = parse(b"\x1b[O");
        assert_eq!(result.n, 3);
        assert_eq!(result.event, Some(Event::FocusOut));
    }

    #[test]
    fn parse_kitty_shift_a_without_text_reporting() {
        let result = parse(b"\x1b[97:65;2u");
        let expected = Event::KeyPress(Key {
            codepoint: u32::from('a'),
            shifted_codepoint: Some(u32::from('A')),
            mods: Modifiers::SHIFT,
            text: Some("A".into()),
            ..Default::default()
        });
        assert_eq!(result.n, 10);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_kitty_alt_shift_a_without_text_reporting() {
        let result = parse(b"\x1b[97:65;4u");
        let expected = Event::KeyPress(Key {
            codepoint: u32::from('a'),
            shifted_codepoint: Some(u32::from('A')),
            mods: Modifiers::SHIFT | Modifiers::ALT,
            ..Default::default()
        });
        assert_eq!(result.n, 10);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_kitty_a_without_text_reporting() {
        let result = parse(b"\x1b[97u");
        let expected = Event::KeyPress(Key {
            codepoint: u32::from('a'),
            ..Default::default()
        });
        assert_eq!(result.n, 5);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_kitty_release_event() {
        let result = parse(b"\x1b[97;1:3u");
        let expected = Event::KeyRelease(Key {
            codepoint: u32::from('a'),
            ..Default::default()
        });
        assert_eq!(result.n, 9);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_single_codepoint() {
        let input = "🙂";
        let result = parse(input.as_bytes());
        let expected = Event::KeyPress(Key {
            codepoint: 0x1F642,
            text: Some(input.into()),
            ..Default::default()
        });
        assert_eq!(result.n, 4);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_single_codepoint_with_more_in_buffer() {
        let result = parse("🙂a".as_bytes());
        let expected = Event::KeyPress(Key {
            codepoint: 0x1F642,
            text: Some("🙂".into()),
            ..Default::default()
        });
        assert_eq!(result.n, 4);
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_multiple_codepoint_grapheme() {
        let input = "👩‍🚀";
        let result = parse(input.as_bytes());
        let expected = Event::KeyPress(Key {
            codepoint: Key::MULTICODEPOINT,
            text: Some(input.into()),
            ..Default::default()
        });
        assert_eq!(result.n, input.len());
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_multiple_codepoint_grapheme_with_more_after() {
        let input = "👩‍🚀abc";
        let result = parse(input.as_bytes());
        let expected_text = "👩‍🚀";
        assert_eq!(result.n, expected_text.len());
        match result.event {
            Some(Event::KeyPress(key)) => {
                assert_eq!(key.text.as_deref(), Some(expected_text));
                assert_eq!(key.codepoint, Key::MULTICODEPOINT);
            }
            other => panic!("expected a key press, got {other:?}"),
        }
    }

    #[test]
    fn parse_flag_emoji() {
        let input = "🇺🇸";
        let result = parse(input.as_bytes());
        let expected = Event::KeyPress(Key {
            codepoint: Key::MULTICODEPOINT,
            text: Some(input.into()),
            ..Default::default()
        });
        assert_eq!(result.n, input.len());
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_combining_mark() {
        // 'a' with combining acute accent (NFD form).
        let input = "a\u{0301}";
        let result = parse(input.as_bytes());
        let expected = Event::KeyPress(Key {
            codepoint: Key::MULTICODEPOINT,
            text: Some(input.into()),
            ..Default::default()
        });
        assert_eq!(result.n, input.len());
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_skin_tone_emoji() {
        let input = "👋🏿";
        let result = parse(input.as_bytes());
        let expected = Event::KeyPress(Key {
            codepoint: Key::MULTICODEPOINT,
            text: Some(input.into()),
            ..Default::default()
        });
        assert_eq!(result.n, input.len());
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_text_variation_selector() {
        // Heavy black heart with text variation selector.
        let input = "❤︎";
        let result = parse(input.as_bytes());
        let expected = Event::KeyPress(Key {
            codepoint: Key::MULTICODEPOINT,
            text: Some(input.into()),
            ..Default::default()
        });
        assert_eq!(result.n, input.len());
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_keycap_sequence() {
        let input = "1️⃣";
        let result = parse(input.as_bytes());
        let expected = Event::KeyPress(Key {
            codepoint: Key::MULTICODEPOINT,
            text: Some(input.into()),
            ..Default::default()
        });
        assert_eq!(result.n, input.len());
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_csi_kitty_multi_cursor() {
        let result = parse(b"\x1b[>1;2;3;29;30;40;100;101 q");
        assert_eq!(result.n, b"\x1b[>1;2;3;29;30;40;100;101 q".len());
        assert_eq!(result.event, Some(Event::CapMultiCursor));

        let result = parse(b"\x1b[> q");
        assert_eq!(result.n, b"\x1b[> q".len());
        assert_eq!(result.event, None);
    }

    #[test]
    fn parse_csi_decrpm() {
        let result = parse(b"\x1b[?1016;1$y");
        assert_eq!(result.n, b"\x1b[?1016;1$y".len());
        assert_eq!(result.event, Some(Event::CapSgrPixels));

        let result = parse(b"\x1b[?1016;0$y");
        assert_eq!(result.n, b"\x1b[?1016;0$y".len());
        assert_eq!(result.event, None);
    }

    #[test]
    fn parse_csi_primary_da() {
        let result = parse(b"\x1b[?c");
        assert_eq!(result.n, b"\x1b[?c".len());
        assert_eq!(result.event, Some(Event::CapDa1));
    }

    #[test]
    fn parse_csi_dsr() {
        let result = parse(b"\x1b[?997;1n");
        assert_eq!(result.n, b"\x1b[?997;1n".len());
        assert_eq!(result.event, Some(Event::ColorScheme(Scheme::Dark)));

        let result = parse(b"\x1b[?997;2n");
        assert_eq!(result.n, b"\x1b[?997;2n".len());
        assert_eq!(result.event, Some(Event::ColorScheme(Scheme::Light)));

        let result = parse(b"\x1b[0n");
        assert_eq!(result.n, b"\x1b[0n".len());
        assert_eq!(result.event, None);
    }

    #[test]
    fn parse_csi_mouse() {
        let result = parse(b"\x1b[<35;1;1m");
        let expected = Event::Mouse(Mouse {
            col: 0,
            row: 0,
            xoffset: 0,
            yoffset: 0,
            button: Button::None,
            kind: Type::Motion,
            mods: crate::mouse::Modifiers::empty(),
        });
        assert_eq!(result.n, b"\x1b[<35;1;1m".len());
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_csi_mouse_negative() {
        let result = parse(b"\x1b[<35;-50;-100m");
        let expected = Event::Mouse(Mouse {
            col: -51,
            row: -101,
            xoffset: 0,
            yoffset: 0,
            button: Button::None,
            kind: Type::Motion,
            mods: crate::mouse::Modifiers::empty(),
        });
        assert_eq!(result.n, b"\x1b[<35;-50;-100m".len());
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_csi_xterm_mouse() {
        let result = parse(b"\x1b[M\x20\x21\x21");
        let expected = Event::Mouse(Mouse {
            col: 0,
            row: 0,
            xoffset: 0,
            yoffset: 0,
            button: Button::Left,
            kind: Type::Press,
            mods: crate::mouse::Modifiers::empty(),
        });
        assert_eq!(result.n, b"\x1b[M\x20\x21\x21".len());
        assert_eq!(result.event, Some(expected));
    }

    #[test]
    fn parse_disambiguate_shift_space() {
        let result = parse(b"\x1b[32;2u");
        let expected = Event::KeyPress(Key {
            codepoint: u32::from(' '),
            shifted_codepoint: Some(u32::from(' ')),
            mods: Modifiers::SHIFT,
            text: Some(" ".into()),
            ..Default::default()
        });
        assert_eq!(result.n, 7);
        assert_eq!(result.event, Some(expected));
    }
}
