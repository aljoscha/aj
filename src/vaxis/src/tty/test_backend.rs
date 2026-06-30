//! An in-memory TTY backend for hermetic renderer tests.

use std::io::{self, Write};

use crate::Winsize;
use crate::tty::{HandlerId, ResizeHandler, Tty};

/// In-memory terminal: writes are captured into an owned `Vec<u8>` and reads
/// drain an injectable input buffer.
///
/// `get_winsize` returns a fixed 40x80 with the same pixel sizes upstream's
/// `TestTty` reports, so renderer tests run against a deterministic geometry.
/// This is the backend phase 6 renderer tests assert against, so the
/// captured-output accessors ([`output`](Self::output),
/// [`take_output`](Self::take_output)) are the ergonomic surface.
#[derive(Debug, Default)]
pub struct TestTty {
    output: Vec<u8>,
    input: Vec<u8>,
    input_pos: usize,
}

impl TestTty {
    /// A backend with no pending input.
    pub fn new() -> Self {
        Self::default()
    }

    /// A backend preloaded with `input` for `read` to return.
    pub fn with_input(input: impl Into<Vec<u8>>) -> Self {
        Self {
            input: input.into(),
            ..Self::default()
        }
    }

    /// The bytes written through [`Tty::writer`] so far.
    pub fn output(&self) -> &[u8] {
        &self.output
    }

    /// Takes the captured output, clearing the buffer.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Appends bytes that subsequent `read` calls will return.
    pub fn inject_input(&mut self, bytes: &[u8]) {
        self.input.extend_from_slice(bytes);
    }
}

impl Tty for TestTty {
    fn writer(&mut self) -> &mut dyn Write {
        &mut self.output
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = &self.input[self.input_pos..];
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.input_pos += n;
        Ok(n)
    }

    fn get_winsize(&self) -> io::Result<Winsize> {
        Ok(Winsize {
            rows: 40,
            cols: 80,
            x_pixel: 40 * 8,
            y_pixel: 40 * 8 * 2,
        })
    }

    fn notify_winsize(&self, _handler: ResizeHandler) -> io::Result<HandlerId> {
        // The test backend never resizes, so registration is a no-op.
        Ok(HandlerId(0))
    }

    fn remove_winsize(&self, _id: HandlerId) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_captures_written_bytes() {
        let mut tty = TestTty::new();
        tty.writer().write_all(b"\x1b[2J").expect("write");
        tty.writer().write_all(b"hi").expect("write");
        assert_eq!(tty.output(), b"\x1b[2Jhi");

        let taken = tty.take_output();
        assert_eq!(taken, b"\x1b[2Jhi");
        assert!(
            tty.output().is_empty(),
            "take_output should clear the buffer"
        );
    }

    #[test]
    fn get_winsize_is_fixed_40x80() {
        let tty = TestTty::new();
        let ws = tty.get_winsize().expect("winsize");
        assert_eq!(
            ws,
            Winsize {
                rows: 40,
                cols: 80,
                x_pixel: 320,
                y_pixel: 640,
            }
        );
    }

    #[test]
    fn read_returns_injected_input() {
        let mut tty = TestTty::with_input(b"abc".to_vec());
        tty.inject_input(b"de");

        let mut buf = [0u8; 4];
        let n = tty.read(&mut buf).expect("read");
        assert_eq!(n, 4);
        assert_eq!(&buf, b"abcd");

        let n = tty.read(&mut buf).expect("read");
        assert_eq!(n, 1);
        assert_eq!(&buf[..1], b"e");

        // Drained: further reads report EOF.
        let n = tty.read(&mut buf).expect("read");
        assert_eq!(n, 0);
    }
}
