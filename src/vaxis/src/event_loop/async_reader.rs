//! The async input front-end: an [`AsyncFd`] over the tty fd driving the shared
//! [`InputCore`], with no reader thread.
//!
//! This is the path the async (tokio) `aj` integration uses. It registers the
//! terminal fd with the tokio reactor, reads when the fd is readable, feeds the
//! same parser/core/runtime the threaded [`Loop`](crate::event_loop::Loop)
//! uses, and emits user events on a [`tokio::sync::mpsc`] channel. The host can
//! `select!` the returned receiver against its own events.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use tokio::io::unix::AsyncFd;
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::event_loop::input::InputCore;
use crate::event_loop::{FromEvent, READ_CHUNK};
use crate::vaxis::Shared;

/// A handle to the spawned async input task.
///
/// The task ends on source EOF, an unrecoverable read error, a closed receiver,
/// or an explicit [`shutdown`](AsyncInput::shutdown). Dropping the handle aborts
/// the task.
pub struct AsyncInput {
    shutdown: Arc<Notify>,
    // `Option` so `join` can take the handle out without fighting `Drop`.
    task: Option<JoinHandle<()>>,
}

impl AsyncInput {
    /// Signals the task to stop after its current iteration.
    pub fn shutdown(&self) {
        self.shutdown.notify_waiters();
    }

    /// Awaits the task's completion (after a [`shutdown`](Self::shutdown) or
    /// EOF).
    pub async fn join(mut self) -> Result<(), tokio::task::JoinError> {
        match self.task.take() {
            Some(task) => task.await,
            None => Ok(()),
        }
    }
}

impl Drop for AsyncInput {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

/// Spawns an async reader over `source` (a terminal fd, e.g. a `/dev/tty`
/// `File`) and returns the event receiver plus a handle.
///
/// `source` is put into non-blocking mode and registered with the tokio
/// reactor. Decoded user events of type `E` are sent on the returned channel;
/// capability and probe responses fold into `shared` exactly as in the threaded
/// loop, so a concurrent [`Vaxis::query_terminal`](crate::vaxis::Vaxis::query_terminal)
/// wakes on DA1.
pub fn async_input<E, S>(
    source: S,
    shared: Arc<Shared>,
) -> io::Result<(UnboundedReceiver<E>, AsyncInput)>
where
    E: FromEvent,
    S: AsRawFd + Send + Sync + 'static,
{
    set_nonblocking(source.as_raw_fd())?;
    let async_fd = AsyncFd::new(source)?;

    let (tx, rx) = mpsc::unbounded_channel::<E>();
    let shutdown = Arc::new(Notify::new());
    let task = tokio::spawn(reader(async_fd, shared, tx, Arc::clone(&shutdown)));

    Ok((
        rx,
        AsyncInput {
            shutdown,
            task: Some(task),
        },
    ))
}

async fn reader<E, S>(
    async_fd: AsyncFd<S>,
    shared: Arc<Shared>,
    tx: UnboundedSender<E>,
    shutdown: Arc<Notify>,
) where
    E: FromEvent,
    S: AsRawFd + Send + Sync,
{
    let mut core = InputCore::new(shared);
    let mut tmp = [0u8; READ_CHUNK];

    loop {
        let mut guard = tokio::select! {
            biased;
            _ = shutdown.notified() => break,
            ready = async_fd.readable() => match ready {
                Ok(guard) => guard,
                // The reactor dropped the registration; nothing more to read.
                Err(_) => break,
            },
        };

        // `try_io` clears readiness and returns `Err` when the read would block
        // (a false-positive wakeup), in which case we loop and await again.
        match guard.try_io(|inner| read_fd(inner.get_ref().as_raw_fd(), &mut tmp)) {
            Ok(Ok(0)) => break, // EOF
            Ok(Ok(n)) => {
                let mut sink = |event: E| {
                    // Ignore send failures: a closed receiver means the host is
                    // gone, handled by the `is_closed` check below.
                    let _ = tx.send(event);
                };
                if core.feed(&tmp[..n], &mut sink).is_err() {
                    break;
                }
            }
            Ok(Err(_)) => break,    // genuine read error
            Err(_would_block) => {} // false readiness; await again
        }

        if tx.is_closed() {
            break;
        }
    }
}

/// Reads from a raw fd, mapping `EAGAIN` to a `WouldBlock` error so `try_io`
/// recognizes a would-block read.
fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    nix::unistd::read(fd, buf).map_err(io::Error::from)
}

/// Puts `fd` into non-blocking mode, required for [`AsyncFd`].
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = fcntl(fd, FcntlArg::F_GETFL).map_err(io::Error::from)?;
    let flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(fd, FcntlArg::F_SETFL(flags)).map_err(io::Error::from)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;

    use super::*;
    use crate::event::Event;
    use crate::key::Key;

    /// Writes `bytes` to the pipe write end, panicking on short write.
    fn write_all(fd: &OwnedFd, bytes: &[u8]) {
        let n = nix::unistd::write(fd, bytes).expect("write to pipe");
        assert_eq!(n, bytes.len(), "short write to pipe");
    }

    #[tokio::test]
    async fn async_reader_decodes_pipe_bytes_into_channel() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");

        let (mut rx, _handle) =
            async_input::<Event, _>(read_fd, Shared::new()).expect("spawn async reader");

        // 'a' then CSI A (cursor up).
        write_all(&write_fd, b"a\x1b[A");

        match rx.recv().await.expect("first event") {
            Event::KeyPress(key) => assert_eq!(key.codepoint, u32::from('a')),
            other => panic!("expected key press, got {other:?}"),
        }
        match rx.recv().await.expect("second event") {
            Event::KeyPress(key) => assert_eq!(key.codepoint, Key::UP),
            other => panic!("expected cursor up, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn async_reader_resyncs_across_pipe_writes() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");

        let (mut rx, _handle) =
            async_input::<Event, _>(read_fd, Shared::new()).expect("spawn async reader");

        // Split a single CSI across two writes: the reader must hold the partial
        // tail and complete it on the second chunk.
        write_all(&write_fd, b"\x1b[");
        write_all(&write_fd, b"A");

        match rx.recv().await.expect("event") {
            Event::KeyPress(key) => assert_eq!(key.codepoint, Key::UP),
            other => panic!("expected cursor up, got {other:?}"),
        }
    }
}
