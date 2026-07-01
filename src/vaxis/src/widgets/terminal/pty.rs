//! A PTY master/slave pair for the embedded terminal emulator.
//!
//! Linux-first, like the rest of the OS layer. [`Pty::open`] allocates the
//! pair through `openpty(3)` (which performs the `/dev/ptmx` unlock and
//! `ptsname` lookup internally) and [`Pty::set_size`] pushes a window size onto
//! the master with `TIOCSWINSZ`. The master fd carries the child's output and
//! accepts our replies and key encodings, the slave fd is what the child
//! process runs on.
//!
//! NOTE: This module is only meaningful on unix. Upstream `@compileError`s on
//! other targets, we simply gate the implementation on `unix` and leave no
//! stub, so a non-unix build sees an empty module.

#![cfg(unix)]

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use nix::pty::openpty;
use thiserror::Error;

use crate::Winsize;

// TIOCSWINSZ is a "bad" ioctl: the request code is a fixed constant rather than
// one derived from the data type, so we use the `_bad` macro variant. It wraps
// `ioctl(2)` and is `unsafe` because it moves a `winsize` through a raw pointer.
nix::ioctl_write_ptr_bad!(tiocswinsz, nix::libc::TIOCSWINSZ, nix::libc::winsize);

/// A failure opening or sizing the PTY.
#[derive(Debug, Error)]
pub enum PtyError {
    /// `openpty(3)` failed.
    #[error("failed to open pty")]
    Open(#[source] std::io::Error),

    /// The `TIOCSWINSZ` ioctl failed.
    #[error("failed to set pty window size")]
    SetSize(#[source] std::io::Error),
}

/// A PTY pair: the `master` we drive and the `slave` the child runs on.
pub struct Pty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

impl Pty {
    /// Opens a fresh PTY pair.
    pub fn open() -> Result<Pty, PtyError> {
        let res = openpty(None, None).map_err(|e| PtyError::Open(e.into()))?;
        Ok(Pty {
            master: res.master,
            slave: res.slave,
        })
    }

    /// Sets the PTY's window size. The ioctl targets the master, which the
    /// kernel propagates to the slave (and delivers a `SIGWINCH` to the child).
    pub fn set_size(&self, ws: Winsize) -> Result<(), PtyError> {
        set_winsize(self.master.as_raw_fd(), ws)
    }
}

/// Applies a [`Winsize`] to an open PTY master fd with `TIOCSWINSZ`.
///
/// Shared by [`Pty::set_size`] and the orchestrator's resize path, which holds
/// the master fd directly after the pair has been split apart.
pub(crate) fn set_winsize(fd: RawFd, ws: Winsize) -> Result<(), PtyError> {
    let winsz = nix::libc::winsize {
        ws_row: ws.rows,
        ws_col: ws.cols,
        ws_xpixel: ws.x_pixel,
        ws_ypixel: ws.y_pixel,
    };
    // SAFETY: `fd` is an open PTY master for the duration of the call, and
    // `winsz` is a valid, correctly-sized input for the ioctl.
    unsafe { tiocswinsz(fd, &winsz).map_err(|e| PtyError::SetSize(e.into()))? };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // TIOCGWINSZ readback, to confirm `set_size` took effect.
    nix::ioctl_read_bad!(tiocgwinsz, nix::libc::TIOCGWINSZ, nix::libc::winsize);

    /// Opens a PTY, returning `None` (with a logged reason) when the sandbox
    /// has no PTY support so the caller can skip.
    fn open_or_skip() -> Option<Pty> {
        match Pty::open() {
            Ok(pty) => Some(pty),
            Err(err) => {
                eprintln!("vaxis: skipping PTY test, open failed: {err}");
                None
            }
        }
    }

    #[test]
    fn set_size_is_reflected_by_tiocgwinsz() {
        let Some(pty) = open_or_skip() else {
            return;
        };
        pty.set_size(Winsize {
            rows: 40,
            cols: 100,
            x_pixel: 800,
            y_pixel: 600,
        })
        .expect("set_size");

        let mut ws = nix::libc::winsize {
            ws_row: 0,
            ws_col: 0,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: the slave is an open tty fd and `ws` is a valid out-param.
        unsafe { tiocgwinsz(pty.slave.as_raw_fd(), &mut ws).expect("tiocgwinsz") };
        assert_eq!(ws.ws_row, 40);
        assert_eq!(ws.ws_col, 100);
        assert_eq!(ws.ws_xpixel, 800);
        assert_eq!(ws.ws_ypixel, 600);
    }
}
