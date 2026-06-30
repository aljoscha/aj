//! The threaded event loop plus the async front-end, over a shared byte-pump.
//!
//! Input decoding lives in the source- and sink-agnostic [`input::InputCore`].
//! Two front-ends drive it:
//!
//! - [`Loop`], the faithful threaded reader over a [`Queue`], ported from
//!   upstream's `Loop`. A `std::thread` drains a [`ByteSource`] into the core
//!   and pushes user events onto the queue.
//! - The async front-end ([`async_input`], unix only), which registers the tty
//!   fd with the tokio reactor and emits user events on a channel with no reader
//!   thread. This is the path the async (tokio) `aj` integration uses; it reuses
//!   the same parser, core, and [`Vaxis`](crate::vaxis::Vaxis) runtime.
//!
//! # The read/write split
//!
//! NOTE: Upstream's `Loop` borrows a `*Tty` and the reader thread reads it while
//! the app writes through the same pointer. Rust cannot soundly alias one
//! `&mut Tty` across the reader thread and the rendering thread, so we split the
//! tty's concerns: the `Loop` owns the read side (a [`ByteSource`]) and a
//! [`WinsizeSource`], while the app keeps the tty's writer for rendering. Reads
//! and writes on a terminal are independent, so this is a faithful adaptation,
//! not a behavior change. A real backend hands the loop a read handle over the
//! same terminal (a dup'd fd or a second open of `/dev/tty`).

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::Winsize;
use crate::event::Event;
use crate::queue::Queue;
use crate::tty::{ResizeHandler, Tty};
use crate::vaxis::Shared;

mod input;

#[cfg(unix)]
mod async_reader;
#[cfg(unix)]
pub use crate::event_loop::async_reader::{AsyncInput, async_input};

/// Read-buffer chunk size for one reader read, matching upstream's 1 KiB.
const READ_CHUNK: usize = 1024;

/// How long the reader sleeps when a source has no bytes available, so it
/// re-checks the quit flag instead of busy-spinning. Small enough that
/// [`Loop::stop`] returns promptly.
const IDLE_POLL: Duration = Duration::from_millis(1);

/// Converts the internal [`Event`] superset into a caller-chosen user event
/// type.
///
/// The threaded [`Loop`] and the async front-end are generic over the user
/// event type. An app can use the full [`Event`] (the identity impl below) or a
/// narrower custom enum that keeps only the variants it handles, which lets the
/// reader drop everything else before it reaches the queue or channel.
///
/// The reader intercepts capability and probe responses (the `cap_*` variants
/// and the F3 explicit-width/scaled-text reports) and never passes them here,
/// so an implementation only handles the user-facing events. The `Send +
/// 'static` bound is what lets a decoded event cross the reader thread or the
/// async channel.
pub trait FromEvent: Sized + Send + 'static {
    /// Converts `event`, or returns `None` to drop it.
    fn from_event(event: Event) -> Option<Self>;
}

impl FromEvent for Event {
    fn from_event(event: Event) -> Option<Self> {
        match event {
            // Internal-only capability variants never reach a user; drop them
            // defensively even though the reader already consumes them.
            Event::CapKittyKeyboard
            | Event::CapKittyGraphics
            | Event::CapRgb
            | Event::CapSgrPixels
            | Event::CapUnicode
            | Event::CapDa1
            | Event::CapColorSchemeUpdates
            | Event::CapMultiCursor => None,
            other => Some(other),
        }
    }
}

/// A blocking source of terminal input bytes the threaded reader drains.
///
/// A blanket impl covers any [`std::io::Read`] that is `Send` (a `File` over
/// `/dev/tty`, a pipe read end), so most backends need no wrapper.
pub trait ByteSource: Send {
    /// Reads input bytes, blocking until some arrive. `Ok(0)` means no bytes are
    /// currently available (end of a scripted source, or a transient idle), in
    /// which case the reader sleeps briefly and re-checks the quit flag.
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
}

impl<R: io::Read + Send> ByteSource for R {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        io::Read::read(self, buf)
    }
}

/// Provides the current window size for the initial winsize event and the
/// SIGWINCH callback. `Send + Sync` so the callback can run on the signal
/// dispatch thread.
pub type WinsizeSource = Arc<dyn Fn() -> io::Result<Winsize> + Send + Sync>;

/// The faithful threaded event loop: a reader thread drains a [`ByteSource`]
/// into the shared core and pushes user events onto a bounded [`Queue`].
///
/// Mirrors upstream's `Loop`: [`start`](Self::start) spawns the reader,
/// [`stop`](Self::stop) signals quit and joins, and the `*_event` methods are
/// the queue surface. The queue capacity is 512, matching upstream.
pub struct Loop<E: FromEvent> {
    queue: Arc<Queue<E, 512>>,
    shared: Arc<Shared>,
    should_quit: Arc<AtomicBool>,
    /// The read source, held while stopped and moved into the reader thread on
    /// [`start`](Self::start). The thread returns it on join, so the loop is
    /// restartable.
    source: Option<Box<dyn ByteSource>>,
    winsize: Option<WinsizeSource>,
    thread: Option<JoinHandle<Box<dyn ByteSource>>>,
    resize_handler: Option<crate::tty::HandlerId>,
}

impl<E: FromEvent> Loop<E> {
    /// Creates a loop reading from `source`, sharing capability/resize state
    /// with the runtime via `shared` (obtain it from
    /// [`Vaxis::shared`](crate::vaxis::Vaxis::shared)).
    pub fn new(source: Box<dyn ByteSource>, shared: Arc<Shared>) -> Self {
        Self {
            queue: Arc::new(Queue::new()),
            shared,
            should_quit: Arc::new(AtomicBool::new(false)),
            source: Some(source),
            winsize: None,
            thread: None,
            resize_handler: None,
        }
    }

    /// Sets the window-size provider. Required for the initial winsize event the
    /// reader posts on start and for [`install_resize_handler`].
    ///
    /// [`install_resize_handler`]: Self::install_resize_handler
    pub fn set_winsize_source(&mut self, winsize: WinsizeSource) {
        self.winsize = Some(winsize);
    }

    /// Spawns the reader thread. A no-op if already running.
    pub fn start(&mut self) {
        if self.thread.is_some() {
            return;
        }
        let Some(source) = self.source.take() else {
            return;
        };
        self.should_quit.store(false, Ordering::Relaxed);

        let queue = Arc::clone(&self.queue);
        let shared = Arc::clone(&self.shared);
        let should_quit = Arc::clone(&self.should_quit);
        let winsize = self.winsize.clone();

        let handle = std::thread::Builder::new()
            .name("vaxis-input".into())
            .spawn(move || reader_loop::<E>(source, shared, queue, should_quit, winsize))
            .expect("spawn vaxis input thread");
        self.thread = Some(handle);
    }

    /// Signals the reader to quit and joins it. A no-op if not running.
    ///
    /// NOTE: A reader blocked in a real (blocking) `ByteSource::read` does not
    /// observe the quit flag until the next byte arrives. Upstream unblocks it
    /// by writing a device-status-report to the tty; in our split the app owns
    /// the writer, so for a blocking real tty the app should trigger such a read
    /// before (or while) calling `stop`. A scripted or non-blocking source (the
    /// test source, a pipe) returns `Ok(0)` and the reader exits within
    /// [`IDLE_POLL`].
    pub fn stop(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        self.should_quit.store(true, Ordering::Relaxed);
        if let Ok(source) = thread.join() {
            // Reclaim the source so the loop can start again.
            self.source = Some(source);
        }
        self.should_quit.store(false, Ordering::Relaxed);
    }

    /// Registers a SIGWINCH resize handler through `tty` (the process-global
    /// registry). The handler posts a winsize event to the queue, suppressed
    /// once in-band resize reports take over. Requires a winsize source.
    pub fn install_resize_handler(&mut self, tty: &dyn Tty) -> io::Result<()> {
        if self.resize_handler.is_some() {
            return Ok(());
        }
        let Some(winsize) = self.winsize.clone() else {
            return Err(io::Error::other(
                "install_resize_handler requires a winsize source",
            ));
        };
        let id = tty.notify_winsize(self.resize_handler(winsize))?;
        self.resize_handler = Some(id);
        Ok(())
    }

    /// Removes a previously installed SIGWINCH handler. A no-op if none is
    /// installed.
    pub fn uninstall_resize_handler(&mut self, tty: &dyn Tty) {
        if let Some(id) = self.resize_handler.take() {
            tty.remove_winsize(id);
        }
    }

    fn resize_handler(&self, winsize: WinsizeSource) -> ResizeHandler {
        let queue = Arc::clone(&self.queue);
        let shared = Arc::clone(&self.shared);
        Arc::new(move || {
            // Once in-band resize reports arrive we receive winsize updates
            // inline, so the out-of-band SIGWINCH path stands down (mirrors
            // upstream's winsizeCallback early-return).
            if shared.in_band_resize() {
                return;
            }
            let Ok(size) = winsize() else {
                return;
            };
            if let Some(event) = E::from_event(Event::Winsize(size)) {
                // Non-blocking: dropping a resize under backpressure beats
                // blocking the signal-dispatch thread.
                let _ = queue.try_push(event);
            }
        })
    }

    /// Returns the next event, blocking until one is available.
    pub fn next_event(&self) -> E {
        self.queue.pop()
    }

    /// Returns the next event if one is available, otherwise `None`.
    pub fn try_event(&self) -> Option<E> {
        self.queue.try_pop()
    }

    /// Blocks until an event is available without removing it (poll + drain).
    pub fn poll_event(&self) {
        self.queue.poll();
    }

    /// Posts an event, blocking if the queue is full.
    pub fn post_event(&self, event: E) {
        self.queue.push(event);
    }

    /// Posts an event without blocking. Returns `false` if the queue is full.
    pub fn try_post_event(&self, event: E) -> bool {
        self.queue.try_push(event)
    }
}

impl<E: FromEvent> Drop for Loop<E> {
    fn drop(&mut self) {
        // Stop the reader so its thread does not outlive the loop. The resize
        // handler (if any) is left registered, since Drop has no tty to
        // unregister it through; callers that care should
        // `uninstall_resize_handler` first.
        self.stop();
    }
}

/// The reader thread body: post the initial winsize, then drain the source into
/// the core until quit, EOF, or an unrecoverable error. Returns the source so
/// the loop can restart.
fn reader_loop<E: FromEvent>(
    mut source: Box<dyn ByteSource>,
    shared: Arc<Shared>,
    queue: Arc<Queue<E, 512>>,
    should_quit: Arc<AtomicBool>,
    winsize: Option<WinsizeSource>,
) -> Box<dyn ByteSource> {
    let mut core = input::InputCore::new(shared);

    // Post the initial (out-of-band) winsize, mirroring upstream's _ttyRun. It
    // does not set the in-band-resize flag, since it comes from an ioctl, not a
    // parsed report.
    if let Some(get) = &winsize {
        if let Ok(size) = get() {
            if let Some(event) = E::from_event(Event::Winsize(size)) {
                queue.push(event);
            }
        }
    }

    let mut tmp = [0u8; READ_CHUNK];
    while !should_quit.load(Ordering::Relaxed) {
        match source.read(&mut tmp) {
            Ok(0) => std::thread::sleep(IDLE_POLL),
            Ok(n) => {
                let mut sink = |event: E| queue.push(event);
                if core.feed(&tmp[..n], &mut sink).is_err() {
                    // A malformed sequence. Upstream ends the reader here (its
                    // ttyRun swallows the error and returns); we do the same
                    // rather than risk re-parsing the same bytes forever.
                    break;
                }
            }
            // Retry an interrupted read, mirroring a syscall EINTR.
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    source
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Arc;

    use super::*;
    use crate::key::Key;
    use crate::tty::TestTty;
    use crate::vaxis::{Options, Vaxis};

    /// A scripted byte source: each `read` returns the next queued chunk, or
    /// `Ok(0)` once drained. Models a tty delivering input in fragments.
    struct ScriptedSource {
        chunks: VecDeque<Vec<u8>>,
    }

    impl ScriptedSource {
        fn new(chunks: impl IntoIterator<Item = Vec<u8>>) -> Self {
            Self {
                chunks: chunks.into_iter().collect(),
            }
        }
    }

    impl ByteSource for ScriptedSource {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let Some(chunk) = self.chunks.pop_front() else {
                return Ok(0);
            };
            let n = chunk.len().min(buf.len());
            buf[..n].copy_from_slice(&chunk[..n]);
            Ok(n)
        }
    }

    /// A narrow custom user event type whose `Foo` variant is absent from the
    /// internal [`Event`], proving the loop is decoupled from the superset.
    #[derive(Debug, PartialEq, Eq)]
    enum UserEvent {
        KeyPress(Key),
        Winsize(Winsize),
        FocusIn,
        Foo(u8),
    }

    impl FromEvent for UserEvent {
        fn from_event(event: Event) -> Option<Self> {
            match event {
                Event::KeyPress(key) => Some(UserEvent::KeyPress(key)),
                Event::Winsize(ws) => Some(UserEvent::Winsize(ws)),
                Event::FocusIn => Some(UserEvent::FocusIn),
                _ => None,
            }
        }
    }

    #[test]
    fn loop_start_stop_and_query_does_not_hang() {
        // The faithful port of upstream `test Loop`. We construct a TestTty (the
        // writer side), a Vaxis, and a Loop over a scripted reader source, then
        // start/stop and run a query. The DA1 reply is injected so the query
        // wakes promptly through the reader -> core -> handshake path.
        let mut tty = TestTty::new();
        let mut vx = Vaxis::new(Options::default());

        let source = Box::new(ScriptedSource::new([b"\x1b[?c".to_vec()]));
        let mut input_loop: Loop<UserEvent> = Loop::new(source, vx.shared());

        input_loop.start();

        // `tty.writer()` is `&mut dyn Write`; `&mut` of it is a sized `W` that
        // implements `Write`, which is what the runtime methods take.
        vx.enter_alt_screen(&mut tty.writer())
            .expect("enter alt screen");
        vx.query_terminal(&mut tty.writer(), Duration::from_secs(1))
            .expect("query terminal");

        input_loop.stop();
    }

    #[test]
    fn post_and_try_event_round_trip() {
        let source = Box::new(ScriptedSource::new([]));
        let input_loop: Loop<UserEvent> = Loop::new(source, Shared::new());

        assert!(input_loop.try_event().is_none());
        input_loop.post_event(UserEvent::Foo(7));
        assert_eq!(input_loop.try_event(), Some(UserEvent::Foo(7)));
        assert!(input_loop.try_event().is_none());
    }

    #[test]
    fn reader_decodes_injected_keys_onto_the_queue() {
        let source = Box::new(ScriptedSource::new([b"ab".to_vec()]));
        let mut input_loop: Loop<UserEvent> = Loop::new(source, Shared::new());
        input_loop.start();

        // Two key presses arrive in order.
        match input_loop.next_event() {
            UserEvent::KeyPress(key) => assert_eq!(key.codepoint, u32::from('a')),
            other => panic!("expected key press, got {other:?}"),
        }
        match input_loop.next_event() {
            UserEvent::KeyPress(key) => assert_eq!(key.codepoint, u32::from('b')),
            other => panic!("expected key press, got {other:?}"),
        }
        input_loop.stop();
    }

    #[test]
    fn resize_handler_posts_winsize_and_honors_in_band_switch() {
        // Exercise the SIGWINCH callback closure directly (no real signal). It
        // posts a winsize event, then stands down once in-band resize is active.
        let shared = Shared::new();
        let mut input_loop: Loop<UserEvent> = Loop::new(Box::new(ScriptedSource::new([])), shared);

        let winsize: WinsizeSource = Arc::new(|| {
            Ok(Winsize {
                rows: 24,
                cols: 80,
                x_pixel: 0,
                y_pixel: 0,
            })
        });
        input_loop.set_winsize_source(Arc::clone(&winsize));

        let handler = input_loop.resize_handler(winsize);
        handler();
        match input_loop.try_event() {
            Some(UserEvent::Winsize(ws)) => assert_eq!(ws.cols, 80),
            other => panic!("expected winsize event, got {other:?}"),
        }

        // Once in-band resize reports arrive, the handler is a no-op.
        input_loop.shared.set_in_band_resize();
        handler();
        assert!(input_loop.try_event().is_none());
    }
}
