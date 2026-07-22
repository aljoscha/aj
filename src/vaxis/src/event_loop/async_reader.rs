//! The async input front-end: a dedicated reader thread doing blocking reads
//! on the tty fd, surfacing decoded events on a channel.
//!
//! This is the path the async (tokio) `aj` integration uses. It reads the
//! terminal on its own thread with a plain blocking `read`, feeds the same
//! parser/core/runtime the threaded [`Loop`](crate::event_loop::Loop) uses, and
//! emits user events on a [`tokio::sync::mpsc`] channel. The host can `select!`
//! the returned receiver against its own events.
//!
//! We read with a blocking `read` rather than registering the fd with an OS
//! reactor, and the reason is a macOS terminal quirk. macOS refuses a
//! freshly-opened `/dev/tty` on both kqueue (what tokio's `AsyncFd` uses) and
//! `poll(2)`, failing with `EINVAL` / `POLLNVAL`. That fd is an alias vnode for
//! the controlling terminal, not the underlying pts, and Darwin's readiness
//! filters will not attach to it. The fd the shell hands us as stdin points
//! straight at the pts and a reactor would accept it (it is what mio/crossterm
//! register), but a reactor also forces the fd non-blocking, and stdin's open
//! file description is shared with the parent shell, so that would leave the
//! shell's stdin non-blocking after we exit. A blocking `read` sidesteps both:
//! it works on any terminal fd and never touches file status flags. It also
//! matches upstream libvaxis, which reads the same way.
//!
//! The fd we read is chosen by
//! [`PosixTty::open_reader`](crate::tty::PosixTty::open_reader): the inherited
//! stdin when it is a terminal (the common case, dodging the alias vnode
//! entirely), and a fresh `/dev/tty` only when stdin is redirected. A blocking
//! read works on both.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::event_loop::input::InputCore;
use crate::event_loop::{FromEvent, READ_CHUNK};
use crate::vaxis::Shared;

/// A handle to the spawned reader thread.
///
/// The thread ends on source EOF, an unrecoverable read error, or a closed
/// receiver. [`shutdown`](AsyncInput::shutdown) sets a quit flag the reader
/// observes after its next read returns.
pub struct AsyncInput {
    quit: Arc<AtomicBool>,
    // `Option` so `join` can take the handle out without fighting `Drop`.
    thread: Option<JoinHandle<()>>,
}

impl AsyncInput {
    /// Asks the reader thread to stop. It observes the flag after its next read
    /// returns, so a reader parked in a blocking `read` keeps waiting until a
    /// byte (or EOF) arrives. Host shutdown does not depend on the thread
    /// stopping promptly: see [`join`](Self::join).
    pub fn shutdown(&self) {
        self.quit.store(true, Ordering::Relaxed);
    }

    /// Releases the reader without waiting for it to finish.
    ///
    /// A blocking `read` on a live terminal cannot be interrupted without input
    /// arriving, and the app owns the writer we would need to provoke one. So
    /// on the host's teardown path we set the quit flag and detach: the process
    /// is exiting, and the OS reaps the parked thread. The read source (an
    /// owned fd) is closed when that thread finally unwinds or the process
    /// exits.
    pub fn join(mut self) {
        self.quit.store(true, Ordering::Relaxed);
        // Detach: dropping the handle does not join, so this returns at once.
        self.thread.take();
    }
}

impl Drop for AsyncInput {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Relaxed);
    }
}

/// Spawns a reader thread over `source` (a terminal fd, e.g. a `/dev/tty`
/// `File`) and returns the event receiver plus a handle.
///
/// `source` is left in blocking mode. Decoded user events of type `E` are sent
/// on the returned channel; capability and probe responses fold into `shared`
/// exactly as in the threaded loop, so a concurrent
/// [`Vaxis::query_terminal`](crate::vaxis::Vaxis::query_terminal) wakes on DA1.
pub fn async_input<E, S>(
    source: S,
    shared: Arc<Shared>,
) -> io::Result<(UnboundedReceiver<E>, AsyncInput)>
where
    E: FromEvent,
    S: AsRawFd + Send + 'static,
{
    let (tx, rx) = mpsc::unbounded_channel::<E>();
    let quit = Arc::new(AtomicBool::new(false));
    let thread = {
        let quit = Arc::clone(&quit);
        std::thread::Builder::new()
            .name("vaxis-input".into())
            .spawn(move || reader(source, shared, tx, quit))?
    };

    Ok((
        rx,
        AsyncInput {
            quit,
            thread: Some(thread),
        },
    ))
}

fn reader<E, S>(source: S, shared: Arc<Shared>, tx: UnboundedSender<E>, quit: Arc<AtomicBool>)
where
    E: FromEvent,
    S: AsRawFd,
{
    let mut core = InputCore::new(shared);
    let mut tmp = [0u8; READ_CHUNK];
    let fd = source.as_raw_fd();

    while !quit.load(Ordering::Relaxed) {
        match read_fd(fd, &mut tmp) {
            Ok(0) => break, // EOF
            Ok(n) => {
                let mut sink = |event: E| {
                    // Ignore send failures: a closed receiver means the host is
                    // gone, handled by the `is_closed` check below.
                    let _ = tx.send(event);
                };
                if core.feed(&tmp[..n], &mut sink).is_err() {
                    break;
                }
            }
            // Retry an interrupted read, mirroring a syscall EINTR.
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break, // genuine read error
        }
        if tx.is_closed() {
            break;
        }
    }
}

/// Reads from a raw fd, blocking until bytes are available.
fn read_fd(fd: RawFd, buf: &mut [u8]) -> io::Result<usize> {
    nix::unistd::read(fd, buf).map_err(io::Error::from)
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

    /// EOF on the source (all write ends closed) ends the reader, which closes
    /// the channel so the host sees `next_input` return `None`.
    #[tokio::test]
    async fn source_eof_closes_the_channel() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        let (mut rx, _handle) =
            async_input::<Event, _>(read_fd, Shared::new()).expect("spawn async reader");
        drop(write_fd);
        assert!(rx.recv().await.is_none(), "channel closes on source EOF");
    }
}
