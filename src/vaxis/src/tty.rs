//! The OS boundary: raw mode, window size, signals, and the buffered writer.
//!
//! The single structural property this layer preserves is that the renderer
//! writes into an abstract buffered writer and the TTY (the fd, raw mode, and
//! signal wiring) is a separate object. The runtime's render methods take a
//! `&mut impl std::io::Write` directly, decoupled from any TTY, so they can be
//! exercised against an in-memory buffer. The [`Tty`] trait here is what the
//! event loop uses to read input, learn the window size, and react to resizes.
//!
//! Backends:
//!
//! - [`PosixTty`] (unix): opens `/dev/tty`, enters raw mode via `nix`, reads
//!   window size with `TIOCGWINSZ`, and routes `SIGWINCH` through `signal-hook`
//!   so no work happens inside the signal handler.
//! - [`TestTty`]: an in-memory backend whose writer captures bytes and whose
//!   window size is a fixed 40x80. Renderer tests assert against it.
//! - `WindowsTty`: a staged stub. The full console backend lands later.

use std::io;
use std::sync::Arc;

use crate::Winsize;

#[cfg(unix)]
mod posix;
mod test_backend;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub use crate::tty::posix::{PosixTty, recover};
pub use crate::tty::test_backend::TestTty;
#[cfg(windows)]
pub use crate::tty::windows::WindowsTty;

/// A callback invoked when the terminal window size changes.
///
/// Handlers run on a normal thread (the `signal-hook` iterator thread for the
/// posix backend), never inside the OS signal handler itself (D7). They must
/// be `Send + Sync` because they are stored in a process-global registry and
/// invoked from that thread.
pub type ResizeHandler = Arc<dyn Fn() + Send + Sync + 'static>;

/// Identifies a registered [`ResizeHandler`] so it can be removed again.
///
/// Returned by [`Tty::notify_winsize`] and consumed by [`Tty::remove_winsize`].
/// Ids are never reused within a process run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HandlerId(u64);

/// The OS terminal boundary the event loop drives.
///
/// The renderer does not go through this trait. It writes to a plain
/// `std::io::Write`, which keeps it testable without a TTY. The trait exists so
/// the loop can read input bytes, query the window size, and register resize
/// callbacks against whichever backend is in use. It is object-safe: `writer`
/// hands back a `&mut dyn Write` rather than a concrete writer type.
pub trait Tty {
    /// The buffered writer for this terminal. The caller flushes it.
    fn writer(&mut self) -> &mut dyn io::Write;

    /// Reads input bytes from the terminal, blocking until some arrive.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;

    /// Queries the current window size from the kernel.
    fn get_winsize(&self) -> io::Result<Winsize>;

    /// Registers a resize callback, returning a [`HandlerId`] for removal.
    ///
    /// Backends with a fixed maximum number of handlers return an error once
    /// full.
    fn notify_winsize(&self, handler: ResizeHandler) -> io::Result<HandlerId>;

    /// Removes a previously registered resize callback. A no-op if the id is
    /// not registered.
    fn remove_winsize(&self, id: HandlerId);
}
