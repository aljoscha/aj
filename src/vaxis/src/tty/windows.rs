//! The Windows console TTY backend.
//!
//! NOTE: this is a staged stub. The full console backend (raw input/output
//! modes, the UTF-8 codepage switch, and `ReadConsoleInputW` event decoding)
//! lands in a later phase, matching upstream's own platform gating. The type
//! exists so the backend module tree is symmetric across platforms. It is
//! gated to `#[cfg(windows)]` and never compiled on unix, so it cannot break
//! non-windows builds.

use std::io::{self, Write};

use crate::Winsize;
use crate::tty::{HandlerId, ResizeHandler, Tty};

/// Placeholder for the Windows console TTY backend.
///
/// `new` returns [`io::ErrorKind::Unsupported`] until the console backend is
/// implemented, so no instance is ever constructed and the [`Tty`] methods are
/// unreachable.
pub struct WindowsTty {
    _private: (),
}

impl WindowsTty {
    /// Always fails: the Windows backend is not implemented yet.
    pub fn new() -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the Windows TTY backend is not yet implemented",
        ))
    }
}

impl Tty for WindowsTty {
    fn writer(&mut self) -> &mut dyn Write {
        unreachable!("WindowsTty cannot be constructed yet")
    }

    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        unreachable!("WindowsTty cannot be constructed yet")
    }

    fn get_winsize(&self) -> io::Result<Winsize> {
        unreachable!("WindowsTty cannot be constructed yet")
    }

    fn notify_winsize(&self, _handler: ResizeHandler) -> io::Result<HandlerId> {
        unreachable!("WindowsTty cannot be constructed yet")
    }

    fn remove_winsize(&self, _id: HandlerId) {
        unreachable!("WindowsTty cannot be constructed yet")
    }
}
