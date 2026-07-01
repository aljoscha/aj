//! The streaming VT parser for a child program's output byte stream.
//!
//! This is the inverse of the crate's top-level input [`crate::parser`]: that
//! one decodes terminal *input* into application events, this one decodes a
//! program's *output* into the operations the emulator applies to its
//! [`crate::widgets::terminal::screen::Screen`].
//!
//! ## Input model
//!
//! [`Parser::parse_reader`] pulls one [`Event`] per call from any
//! [`std::io::BufRead`]. Tests feed a byte slice through a
//! [`std::io::Cursor`], whose `BufRead` returns all remaining bytes at once,
//! and pull events until [`ParseError::Eof`]. This reproduces upstream's
//! "flush the accumulated print run when the reader's buffer drains or a
//! control byte is hit" rule: a print event ends as soon as no more bytes are
//! immediately available (`fill_buf` empty) or a control byte appears.
//!
//! ## Pending byte
//!
//! A ground (print) run stops at the first control byte, which cannot be part
//! of the run. That byte is stashed in [`Parser::pending_byte`] and consumed
//! first on the next call, so the control sequence it begins is parsed intact.

use std::io::BufRead;

use thiserror::Error;

use crate::widgets::terminal::ansi::{C0, Csi};

/// A single terminal event decoded from the output stream.
///
/// `print`, `escape`, `osc`, and `apc` carry raw bytes. A print run always
/// holds complete UTF-8 sequences (the parser reads whole codepoints), so it
/// is valid UTF-8 when the input is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Print(Vec<u8>),
    C0(C0),
    Escape(Vec<u8>),
    Ss2(u8),
    Ss3(u8),
    Csi(Csi),
    Osc(Vec<u8>),
    Apc(Vec<u8>),
}

/// A failure while parsing the output stream.
#[derive(Debug, Error)]
pub enum ParseError {
    /// A read on the underlying reader failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// The reader was exhausted mid-event (or between events). Callers driving
    /// a finite byte slice treat this as "no more events".
    #[error("unexpected end of input")]
    Eof,

    /// A print run began with a byte that cannot start a UTF-8 sequence.
    #[error("invalid utf-8 start byte: {0:#04x}")]
    InvalidUtf8StartByte(u8),
}

/// The streaming VT parser. Holds the accumulator for the event under
/// construction plus the one-byte lookahead described in the module docs.
#[derive(Debug, Default)]
pub struct Parser {
    buf: Vec<u8>,
    /// A leftover control byte from a ground run, consumed first next call.
    pending_byte: Option<u8>,
}

impl Parser {
    /// Creates a parser with an empty accumulator and no pending byte.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pulls the next event from `reader`.
    ///
    /// Returns [`ParseError::Eof`] when the reader is exhausted, which for a
    /// finite input marks the end of the event stream.
    pub fn parse_reader<R: BufRead>(&mut self, reader: &mut R) -> Result<Event, ParseError> {
        self.buf.clear();
        loop {
            let b = match self.pending_byte.take() {
                Some(p) => p,
                None => take_byte(reader)?,
            };
            match b {
                // Escape sequence: dispatch on the byte after ESC.
                0x1b => {
                    let next = take_byte(reader)?;
                    match next {
                        0x4e => return Ok(Event::Ss2(take_byte(reader)?)),
                        0x4f => return Ok(Event::Ss3(take_byte(reader)?)),
                        // DCS, SOS, PM: skip to the string terminator, then
                        // resume the loop. Upstream does not return here, so a
                        // trailing byte left by `skip_until_st` (see its NOTE)
                        // is parsed as the next event.
                        0x50 | 0x58 | 0x5e => {
                            skip_until_st(reader)?;
                            continue;
                        }
                        0x5b => return self.parse_csi(reader),
                        0x5d => return self.parse_osc(reader),
                        0x5f => return self.parse_apc(reader),
                        // ESC with intermediates.
                        0x20..=0x2f => {
                            self.buf.push(next);
                            return self.parse_escape(reader);
                        }
                        _ => {
                            self.buf.push(next);
                            return Ok(Event::Escape(self.buf.clone()));
                        }
                    }
                }
                // C0 control (ESC handled above).
                0x00..=0x1a | 0x1c..=0x1f => {
                    return Ok(Event::C0(
                        C0::from_u8(b).expect("byte is in the C0 control range"),
                    ));
                }
                // Ground: begin a print run.
                _ => {
                    self.buf.push(b);
                    return self.parse_ground(reader);
                }
            }
        }
    }

    /// Accumulates printable bytes into a print run.
    ///
    /// Reads whole UTF-8 sequences (length taken from the leading byte) and
    /// stops when the reader's buffer drains or a control byte is hit, stashing
    /// that control byte in [`Parser::pending_byte`].
    fn parse_ground<R: BufRead>(&mut self, reader: &mut R) -> Result<Event, ParseError> {
        debug_assert!(!self.buf.is_empty());

        // Finish the first codepoint's continuation bytes.
        let len = utf8_len(self.buf[0])?;
        for _ in 1..len {
            self.buf.push(take_byte(reader)?);
        }

        loop {
            if buffered_len(reader)? == 0 {
                return Ok(Event::Print(self.buf.clone()));
            }
            let b = take_byte(reader)?;
            match b {
                0x00..=0x1f => {
                    self.pending_byte = Some(b);
                    return Ok(Event::Print(self.buf.clone()));
                }
                _ => {
                    self.buf.push(b);
                    let len = utf8_len(b)?;
                    for _ in 1..len {
                        self.buf.push(take_byte(reader)?);
                    }
                }
            }
        }
    }

    /// Parses an escape sequence with intermediate bytes: intermediates in
    /// `0x20..=0x2f` are skipped, and the first byte outside that range is the
    /// final byte.
    fn parse_escape<R: BufRead>(&mut self, reader: &mut R) -> Result<Event, ParseError> {
        loop {
            let b = take_byte(reader)?;
            match b {
                0x20..=0x2f => continue,
                _ => {
                    self.buf.push(b);
                    return Ok(Event::Escape(self.buf.clone()));
                }
            }
        }
    }

    /// Parses an APC sequence, terminated by ST (`ESC \`). Certain control
    /// bytes are ignored, the rest accumulate into the payload.
    fn parse_apc<R: BufRead>(&mut self, reader: &mut R) -> Result<Event, ParseError> {
        loop {
            let b = take_byte(reader)?;
            match b {
                0x00..=0x17 | 0x19 | 0x1c..=0x1f => continue,
                0x1b => {
                    // Consume the `\` of the ST, tolerating EOF.
                    discard_one(reader)?;
                    return Ok(Event::Apc(self.buf.clone()));
                }
                _ => self.buf.push(b),
            }
        }
    }

    /// Parses an OSC sequence, terminated by either BEL (`0x07`) or ST
    /// (`ESC \`).
    fn parse_osc<R: BufRead>(&mut self, reader: &mut R) -> Result<Event, ParseError> {
        loop {
            let b = take_byte(reader)?;
            match b {
                0x07 => return Ok(Event::Osc(self.buf.clone())),
                0x1b => {
                    discard_one(reader)?;
                    return Ok(Event::Osc(self.buf.clone()));
                }
                0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1c..=0x1f => continue,
                _ => self.buf.push(b),
            }
        }
    }

    /// Parses a CSI sequence: it collects intermediates (`0x20..=0x2f`),
    /// parameter bytes (`0x30..=0x3b`), and one private marker (`0x3c..=0x3f`),
    /// terminating on the final byte (`0x40..=0xff`). C0 controls encountered
    /// mid-sequence are ignored.
    fn parse_csi<R: BufRead>(&mut self, reader: &mut R) -> Result<Event, ParseError> {
        let mut intermediate = None;
        let mut private_marker = None;
        loop {
            let b = take_byte(reader)?;
            match b {
                0x20..=0x2f => intermediate = Some(b),
                0x30..=0x3b => self.buf.push(b),
                0x3c..=0x3f => private_marker = Some(b),
                0x40..=0xff => {
                    return Ok(Event::Csi(Csi {
                        intermediate,
                        private_marker,
                        final_byte: b,
                        params: self.buf.clone(),
                    }));
                }
                _ => continue,
            }
        }
    }
}

/// Reads exactly one byte, returning [`ParseError::Eof`] at end of input.
fn take_byte<R: BufRead>(reader: &mut R) -> Result<u8, ParseError> {
    let buf = reader.fill_buf()?;
    let Some(&b) = buf.first() else {
        return Err(ParseError::Eof);
    };
    reader.consume(1);
    Ok(b)
}

/// Discards up to one byte, tolerating end of input.
///
/// Mirrors upstream's `discard(limited(1))`, which returns a count rather than
/// erroring, so a sequence that ends exactly at the ST's ESC does not fail.
fn discard_one<R: BufRead>(reader: &mut R) -> Result<(), ParseError> {
    let buf = reader.fill_buf()?;
    if buf.is_empty() {
        return Ok(());
    }
    reader.consume(1);
    Ok(())
}

/// Returns the number of bytes immediately available without a further read.
fn buffered_len<R: BufRead>(reader: &mut R) -> Result<usize, ParseError> {
    Ok(reader.fill_buf()?.len())
}

/// Skips bytes until an ST (`ESC \`).
///
/// NOTE: This discards up to (but not including) the ESC, then discards the
/// ESC itself, leaving the following `\` in the stream. That trailing byte is
/// parsed as the next event. This faithfully reproduces upstream, where the
/// DCS/SOS/PM branches call this and then resume the parse loop rather than
/// consuming the whole terminator.
fn skip_until_st<R: BufRead>(reader: &mut R) -> Result<(), ParseError> {
    discard_until_exclusive(reader, 0x1b)?;
    discard_one(reader)?;
    Ok(())
}

/// Discards bytes until `delim` is the next byte, without consuming `delim`.
/// Returns [`ParseError::Eof`] if the input ends before `delim` is found.
fn discard_until_exclusive<R: BufRead>(reader: &mut R, delim: u8) -> Result<(), ParseError> {
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            return Err(ParseError::Eof);
        }
        match buf.iter().position(|&x| x == delim) {
            Some(pos) => {
                reader.consume(pos);
                return Ok(());
            }
            None => {
                let n = buf.len();
                reader.consume(n);
            }
        }
    }
}

/// Returns the UTF-8 sequence length implied by a leading byte, or
/// [`ParseError::InvalidUtf8StartByte`] for a byte that cannot start one.
fn utf8_len(first: u8) -> Result<usize, ParseError> {
    match first {
        0x00..=0x7f => Ok(1),
        0xc0..=0xdf => Ok(2),
        0xe0..=0xef => Ok(3),
        0xf0..=0xf7 => Ok(4),
        _ => Err(ParseError::InvalidUtf8StartByte(first)),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    /// Parses `input` to exhaustion, returning every event in order.
    fn parse_all(input: &[u8]) -> Vec<Event> {
        let mut parser = Parser::new();
        let mut cursor = Cursor::new(input);
        let mut events = Vec::new();
        loop {
            match parser.parse_reader(&mut cursor) {
                Ok(ev) => events.push(ev),
                Err(ParseError::Eof) => break,
                Err(e) => panic!("unexpected parse error: {e}"),
            }
        }
        events
    }

    fn print(s: &str) -> Event {
        Event::Print(s.as_bytes().to_vec())
    }

    #[test]
    fn plain_text_is_one_print() {
        assert_eq!(parse_all(b"hello"), vec![print("hello")]);
    }

    #[test]
    fn csi_cursor_position() {
        assert_eq!(
            parse_all(b"\x1b[1;2H"),
            vec![Event::Csi(Csi {
                intermediate: None,
                private_marker: None,
                final_byte: b'H',
                params: b"1;2".to_vec(),
            })]
        );
    }

    #[test]
    fn csi_sgr_256_color() {
        assert_eq!(
            parse_all(b"\x1b[38;5;9m"),
            vec![Event::Csi(Csi {
                intermediate: None,
                private_marker: None,
                final_byte: b'm',
                params: b"38;5;9".to_vec(),
            })]
        );
    }

    #[test]
    fn csi_private_marker_and_intermediate() {
        assert_eq!(
            parse_all(b"\x1b[?25h"),
            vec![Event::Csi(Csi {
                intermediate: None,
                private_marker: Some(b'?'),
                final_byte: b'h',
                params: b"25".to_vec(),
            })]
        );
    }

    #[test]
    fn osc_bel_terminated() {
        assert_eq!(
            parse_all(b"\x1b]0;title\x07"),
            vec![Event::Osc(b"0;title".to_vec())]
        );
    }

    #[test]
    fn osc_st_terminated() {
        // OSC 7 with an ST (ESC \) terminator.
        assert_eq!(
            parse_all(b"\x1b]7;file://host/path\x1b\\"),
            vec![Event::Osc(b"7;file://host/path".to_vec())]
        );
    }

    #[test]
    fn apc_st_terminated() {
        // APC fully consumes the ST, so nothing leaks after it.
        assert_eq!(
            parse_all(b"\x1b_Gi=1,a=T;payload\x1b\\"),
            vec![Event::Apc(b"Gi=1,a=T;payload".to_vec())]
        );
    }

    #[test]
    fn c0_bytes() {
        assert_eq!(
            parse_all(b"\x07\x08\r"),
            vec![Event::C0(C0::Bel), Event::C0(C0::Bs), Event::C0(C0::Cr)]
        );
    }

    #[test]
    fn ss2_and_ss3() {
        assert_eq!(
            parse_all(b"\x1bNA\x1bOB"),
            vec![Event::Ss2(b'A'), Event::Ss3(b'B')]
        );
    }

    #[test]
    fn escape_with_intermediate() {
        // ESC ( B: designate ASCII into G0. One escape event ending at 'B'.
        assert_eq!(parse_all(b"\x1b(B"), vec![Event::Escape(b"(B".to_vec())]);
    }

    #[test]
    fn escape_bare() {
        // ESC = (application keypad) has no intermediates.
        assert_eq!(parse_all(b"\x1b="), vec![Event::Escape(b"=".to_vec())]);
    }

    #[test]
    fn print_run_split_by_control_byte() {
        // The BEL between the two runs stops the first print and comes back as
        // a C0 event via the pending-byte path.
        assert_eq!(
            parse_all(b"ab\x07cd"),
            vec![print("ab"), Event::C0(C0::Bel), print("cd")]
        );
    }

    #[test]
    fn print_then_csi() {
        // ESC ends the print run and begins a CSI. The pending byte is the ESC.
        assert_eq!(
            parse_all(b"hi\x1b[0m"),
            vec![
                print("hi"),
                Event::Csi(Csi {
                    intermediate: None,
                    private_marker: None,
                    final_byte: b'm',
                    params: b"0".to_vec(),
                }),
            ]
        );
    }

    #[test]
    fn multibyte_utf8_print() {
        // "café" plus a wide CJK char, all in one print run.
        assert_eq!(parse_all("café漢".as_bytes()), vec![print("café漢")]);
    }

    #[test]
    fn dcs_is_skipped_leaving_st_backslash() {
        // A DCS is skipped; the ST's trailing '\' is parsed as a print. This
        // pins the documented skip_until_st behavior.
        assert_eq!(parse_all(b"\x1bPq;data\x1b\\X"), vec![print("\\X")]);
    }
}
