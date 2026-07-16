//! The async, host-driven vxfw runtime.
//!
//! [`AsyncApp`] owns the terminal and the vxfw engine but not the loop: the
//! host multiplexes terminal input against its own event sources with
//! `tokio::select!` and calls back into the driver for dispatch and drawing.
//! A typical host loop:
//!
//! ```ignore
//! app.init(root.clone(), Options::default()).await?;
//! loop {
//!     tokio::select! {
//!         Some(ev) = app.next_input() => {
//!             if app.handle_input(ev).quit { break; }
//!         }
//!         _ = sleep_until_next_deadline(&app) => { app.fire_due_timers(); }
//!         // ... the host's own channels, each arm ending in
//!         // app.request_redraw() when it changed what the widgets show.
//!     }
//!     app.render_if_needed(&root)?;
//! }
//! app.shutdown().await;
//! ```
//!
//! The futures here are `!Send` ([`WidgetRef`] is an `Rc`), so drive them from
//! a top-level `block_on` or a `tokio::task::LocalSet`.

use std::io::Write;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::{Duration, Instant};

use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::error::Error;
use crate::event_loop::{AsyncInput, async_input};
use crate::image::{Image, Source};
use crate::tty::Tty;
use crate::vaxis::Vaxis;
use crate::vxfw::app_core::{
    AppCore, FocusHandler, FrameStats, KeystrokeRecord, MouseHandler, reset_event_state,
};
use crate::vxfw::loop_event::LoopEvent;
use crate::vxfw::{Event, EventContext, Options, UserEvent, WidgetRef};

/// The outcome of one dispatch step.
#[derive(Debug, Clone, Copy)]
pub struct Frame {
    /// A handler asked to quit the application.
    pub quit: bool,
}

/// Panic message for driver methods used before [`AsyncApp::init`].
const NOT_INITIALIZED: &str = "AsyncApp::init must be called before driving the app";

/// State [`AsyncApp::init`] creates: the async reader, the dispatch handlers,
/// and the out-of-band resize stream.
struct Running {
    input_rx: UnboundedReceiver<LoopEvent>,
    input: AsyncInput,
    mouse: MouseHandler,
    focus: FocusHandler,
    /// The SIGWINCH stream, installed only when the terminal does not report
    /// resizes in-band.
    sigwinch: Option<Signal>,
}

/// Applies a queued focus request and rebuilds its dispatch path immediately.
///
/// Focus can move between rendered frames in the host-driven runtime. The
/// fresh layout prevents the next queued event from walking the old widget
/// path. It updates dispatch state only, without painting or clearing redraw.
fn apply_pending_focus(core: &mut AppCore, ctx: &mut EventContext, running: &mut Running) {
    let Some(widget) = core.wants_focus.take() else {
        return;
    };
    running.focus.focus_widget(ctx, widget);
    core.handle_command(&mut ctx.cmds);
    let root = Rc::clone(&running.focus.root);
    let surface = core.do_layout(&root);
    running.focus.update(&surface);
}

/// The host-driven widget-framework driver: the shared vxfw engine behind an
/// async, per-step API.
///
/// Lifecycle contract: call [`init`](AsyncApp::init) exactly once before any
/// other method (the dispatch and render methods panic otherwise), and call
/// [`shutdown`](AsyncApp::shutdown) on the way out to stop the reader and
/// restore the terminal. On an unclean drop the reader task aborts and the
/// posix tty restores termios, but alt screen, mouse, and keyboard state stay
/// on (the panic hook covers panics).
///
/// The redraw latch is per-frame, not per-event: it persists across
/// [`handle_input`](AsyncApp::handle_input) /
/// [`fire_due_timers`](AsyncApp::fire_due_timers) calls and clears when a
/// render draws, while the per-event state (consume, phase) resets after every
/// dispatch.
pub struct AsyncApp {
    core: AppCore,
    /// The read source, consumed by [`init`](AsyncApp::init) when it spawns
    /// the async reader.
    source: Option<OwnedFd>,
    ctx: EventContext,
    running: Option<Running>,
}

impl AsyncApp {
    /// Builds a driver over a runtime, a writer-side tty, and a read source.
    ///
    /// The read source must be a separate open file description of the
    /// terminal (see
    /// [`PosixTty::open_reader`](crate::tty::PosixTty::open_reader)), not a
    /// dup of the writer: the async reader flips its fd non-blocking, and a
    /// dup would flip the writer with it.
    pub fn new(vx: Vaxis, tty: Box<dyn Tty>, source: OwnedFd) -> AsyncApp {
        AsyncApp {
            core: AppCore::new(vx, tty),
            source: Some(source),
            ctx: EventContext::new(),
            running: None,
        }
    }

    /// Terminal setup plus the first frame: spawns the async reader, runs
    /// capability detection off the executor, enters the alt screen, enables
    /// mouse and bracketed paste, dispatches `Init` and `FocusIn`, and draws.
    ///
    /// `opts` is accepted for parity with [`App::run`](crate::vxfw::App::run);
    /// its framerate is unused because the host paces frames. Panics if called
    /// twice.
    pub async fn init(&mut self, root: WidgetRef, _opts: Options) -> Result<(), Error> {
        // Size the screen from the tty before the first layout so the
        // cell-size division in layout has a non-zero denominator. The async
        // reader posts no initial winsize event, so this live read is the only
        // initial sizing.
        let ws = self.core.tty.get_winsize()?;
        self.core.vx.resize(&mut self.core.tty.writer(), ws)?;

        // Spawn the reader before sending the probe batch: the reader is what
        // consumes the probe replies and fires the DA1 handshake, so sending
        // first would push the wait below into its timeout.
        let source = self.source.take().expect("AsyncApp::init called twice");
        let (input_rx, input) = async_input::<LoopEvent, _>(source, self.core.vx.shared())?;

        self.core.vx.enter_alt_screen(&mut self.core.tty.writer())?;
        self.core
            .vx
            .query_terminal_send(&mut self.core.tty.writer())?;
        // Park a blocking-pool thread on the DA1 condvar instead of blocking
        // the runtime. The reader folds capability replies into the shared
        // state and notifies the condvar when DA1 arrives, or we give up after
        // the timeout and run with the capabilities detected so far.
        let shared = self.core.vx.shared();
        tokio::task::spawn_blocking(move || shared.wait_for_da1(Duration::from_secs(1)))
            .await
            .expect("DA1 wait task panicked");
        self.core
            .vx
            .query_terminal_finish(&mut self.core.tty.writer())?;
        self.core
            .vx
            .set_bracketed_paste(&mut self.core.tty.writer(), true)?;
        self.core
            .vx
            .subscribe_to_color_scheme_updates(&mut self.core.tty.writer())?;

        // Only install the out-of-band SIGWINCH stream when the terminal does
        // not report resizes in-band. We wait until detection finished (above)
        // to decide.
        let sigwinch = if self.core.vx.shared().in_band_resize() {
            None
        } else {
            Some(signal(SignalKind::window_change())?)
        };

        // We do not use pixel mouse, so force it off before enabling mouse mode.
        self.core.vx.caps.sgr_pixels = false;
        self.core
            .vx
            .set_mouse_mode(&mut self.core.tty.writer(), true)?;

        let mouse = MouseHandler::init(Rc::clone(&root));
        let mut focus = FocusHandler::init(Rc::clone(&root));
        focus.path_to_focused.push(Rc::clone(&root));
        self.running = Some(Running {
            input_rx,
            input,
            mouse,
            focus,
            sigwinch,
        });

        // Always start the app with an init event and a focus event.
        self.handle_input(Event::Init);
        self.handle_input(Event::FocusIn);

        // The first frame draws unconditionally.
        self.render(&root)
    }

    /// Awaits the next terminal input event. Returns `None` when the reader
    /// has ended (source EOF or a read error).
    ///
    /// Out-of-band SIGWINCH folds in here as a synthesized [`Event::Winsize`]
    /// read live from the tty, so the host sees one input stream whether or
    /// not the terminal reports resizes in-band.
    pub async fn next_input(&mut self) -> Option<Event> {
        let running = self.running.as_mut().expect(NOT_INITIALIZED);
        loop {
            // The select arms only bind an outcome. We act on it after the
            // select so no arm mutates state the other arm's future borrows.
            enum Arm {
                Input(Option<LoopEvent>),
                Sigwinch(Option<()>),
            }
            let has_sigwinch = running.sigwinch.is_some();
            let arm = tokio::select! {
                event = running.input_rx.recv() => Arm::Input(event),
                fired = async {
                    running
                        .sigwinch
                        .as_mut()
                        .expect("arm guarded on is_some")
                        .recv()
                        .await
                }, if has_sigwinch => Arm::Sigwinch(fired),
            };
            match arm {
                Arm::Input(event) => return event.map(LoopEvent::into_event),
                // The signal stream ended. Stop selecting on it.
                Arm::Sigwinch(None) => running.sigwinch = None,
                Arm::Sigwinch(Some(())) => {
                    // A live ioctl so the reported size is the current one,
                    // not a snapshot from before the resize. A failed ioctl
                    // drops this resize and keeps waiting for input.
                    if let Ok(ws) = self.core.tty.get_winsize() {
                        return Some(Event::Winsize(ws));
                    }
                }
            }
        }
    }

    /// Runs one input event through the engine: mouse hit-test plus
    /// enter/leave plus capture/target/bubble, or focus-path dispatch, or an
    /// internal resize. Applies the resulting commands and any focus change.
    /// Sets the redraw latch as widgets request it.
    pub fn handle_input(&mut self, event: Event) -> Frame {
        let running = self.running.as_mut().expect(NOT_INITIALIZED);
        match &event {
            Event::Mouse(mouse) => {
                running
                    .mouse
                    .handle_mouse(&mut self.core, &mut self.ctx, *mouse)
            }
            Event::FocusOut => {
                running.mouse.mouse_exit(&mut self.core, &mut self.ctx);
                running.focus.handle_event(&mut self.ctx, &event);
                self.core.handle_command(&mut self.ctx.cmds);
            }
            Event::Winsize(ws) => {
                // A resize failure is a tty write failure. We swallow it here
                // so the dispatch API stays infallible. The next render writes
                // to the same tty and reports it.
                let _ = self.core.vx.resize(&mut self.core.tty.writer(), *ws);
                self.ctx.redraw = true;
            }
            _ => {
                running.focus.handle_event(&mut self.ctx, &event);
                self.core.handle_command(&mut self.ctx.cmds);
            }
        }
        // Per-event reset (defer semantics): clears consume_event and the
        // phase but leaves the per-frame redraw latch.
        reset_event_state(&mut self.ctx);

        // Apply a focus change requested by a handler, and drain the commands
        // the focus events produced.
        apply_pending_focus(&mut self.core, &mut self.ctx, running);
        Frame {
            quit: self.ctx.quit,
        }
    }

    /// Dispatches a host event to the focused widget as [`Event::App`].
    pub fn post_app_event(&mut self, event: UserEvent) -> Frame {
        self.handle_input(Event::App(event))
    }

    /// The soonest pending tick deadline, for the host's timer select arm.
    pub fn next_tick_deadline(&self) -> Option<Instant> {
        // `timers` is kept sorted with the soonest deadline last.
        self.core.timers.last().map(|t| t.deadline)
    }

    /// A snapshot of recent frame-render statistics, for a host debug overlay.
    ///
    /// Reads state only and is cheap, so a host can call it from inside the
    /// overlay widget while drawing. The values describe the frames before the
    /// one being drawn, so the overlay is always one frame behind, which is
    /// correct. The profiler runs always-on.
    pub fn frame_stats(&self) -> FrameStats {
        self.core.frame_stats()
    }

    /// Fires every due tick, delivering [`Event::Tick`], and applies the
    /// resulting commands and any focus change.
    pub fn fire_due_timers(&mut self) -> Frame {
        let running = self.running.as_mut().expect(NOT_INITIALIZED);
        self.core.check_timers(&mut self.ctx);
        // A tick handler may request focus without requesting a redraw, so we
        // apply the change here rather than leaving it to the next input or
        // render.
        apply_pending_focus(&mut self.core, &mut self.ctx, running);
        Frame {
            quit: self.ctx.quit,
        }
    }

    /// Marks the frame dirty so the next [`render_if_needed`] draws.
    ///
    /// [`render_if_needed`]: AsyncApp::render_if_needed
    pub fn request_redraw(&mut self) {
        self.ctx.redraw = true;
    }

    /// Whether a redraw is pending.
    pub fn needs_redraw(&self) -> bool {
        self.ctx.redraw
    }

    /// Lays out the root, updates mouse and focus state against the fresh
    /// surface, and diff-renders to the tty. Clears the redraw latch.
    pub fn render(&mut self, root: &WidgetRef) -> Result<(), Error> {
        let running = self.running.as_mut().expect(NOT_INITIALIZED);
        self.ctx.redraw = false;
        debug_assert!(self.ctx.cmds.is_empty());

        let mut surface = self.core.do_layout(root);
        // Updating the mouse against the fresh surface may change hover state
        // and request another redraw.
        running
            .mouse
            .update_mouse(&mut self.core, &surface, &mut self.ctx);
        if let Some(widget) = self.core.wants_focus.take() {
            running.focus.focus_widget(&mut self.ctx, widget);
            self.core.handle_command(&mut self.ctx.cmds);
        }
        debug_assert!(self.ctx.cmds.is_empty());
        if self.ctx.redraw {
            // The mouse or focus updates dirtied the tree again. Re-lay-out
            // for this draw and leave the latch set so the next
            // render_if_needed draws the settled state, matching the
            // synchronous frame loop.
            surface = self.core.do_layout(root);
        }

        running.mouse.last_frame = surface;
        running.focus.update(&running.mouse.last_frame);
        let focused = Rc::clone(&running.focus.focused);
        self.core.render(&running.mouse.last_frame, &focused)
    }

    /// Calls [`render`](AsyncApp::render) only when a redraw is pending. The
    /// common per-iteration call.
    pub fn render_if_needed(&mut self, root: &WidgetRef) -> Result<(), Error> {
        if self.ctx.redraw {
            self.render(root)?;
        }
        Ok(())
    }

    /// The underlying runtime, for host needs the
    /// [`Command`](crate::vxfw::Command) enum does not cover.
    pub fn vaxis(&mut self) -> &mut Vaxis {
        &mut self.core.vx
    }

    /// The dispatch-debug log of recent key presses (the last ~100), oldest
    /// first. Each record carries the focus path the key walked and where (if
    /// anywhere) it was consumed.
    pub fn keystroke_log(&self) -> impl Iterator<Item = &KeystrokeRecord> {
        self.running
            .as_ref()
            .expect(NOT_INITIALIZED)
            .focus
            .keystroke_log
            .iter()
    }

    /// Runs `f` with the tty's buffered writer, for custom escape sequences.
    /// The caller flushes if the bytes must go out immediately.
    pub fn with_writer<R>(&mut self, f: impl FnOnce(&mut dyn Write) -> R) -> R {
        f(self.core.tty.writer())
    }

    /// Transmit an image into the terminal's graphics store, returning a handle
    /// whose id can be placed into a surface cell. Requires the kitty graphics
    /// capability, returning [`Error::NoGraphicsCapability`] otherwise.
    //
    // NOTE: This lives here rather than being composed from `vaxis()` and
    // `with_writer()` because transmission borrows the `Vaxis` and the tty
    // writer at once, and those are disjoint `pub(crate)` fields of `AppCore`
    // that no external caller can borrow together.
    pub fn load_image(&mut self, source: Source) -> Result<Image, Error> {
        let mut writer = self.core.tty.writer();
        self.core.vx.load_image(&mut writer, source)
    }

    /// Delete a transmitted image from the terminal's graphics store, freeing
    /// its id. Best-effort: write and flush errors are swallowed, matching
    /// [`Vaxis::free_image`].
    pub fn free_image(&mut self, id: u32) {
        let mut writer = self.core.tty.writer();
        self.core.vx.free_image(&mut writer, id);
    }

    /// Restores the terminal: stops the reader, leaves the alt screen,
    /// disables mouse and paste, shows the cursor, and flushes. Best-effort.
    pub async fn shutdown(mut self) {
        // The async reader parks on a shutdown Notify rather than a blocking
        // read, so no wake byte is needed: signal and join.
        if let Some(running) = self.running.take() {
            running.input.shutdown();
            let _ = running.input.join().await;
        }
        // Best-effort restore, and flush because the writer is buffered and
        // the reset bytes must reach the terminal before the host prints to
        // stdout.
        let _ = self.core.vx.reset_state(&mut self.core.tty.writer());
        let _ = self.core.tty.writer().flush();
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::os::fd::OwnedFd;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::Winsize;
    use crate::cell::{Cell, Character};
    use crate::tty::TestTty;
    use crate::vaxis::Options as VaxisOptions;
    use crate::vxfw::app_core::FRAME_STATS_CAP;
    use crate::vxfw::{
        DrawContext, RelativePoint, SubSurface, Surface, Tick, Widget, draw_widget, to_widget_ref,
    };

    /// A root widget that records the keys and ticks it receives, consuming
    /// each and requesting a redraw.
    #[derive(Default)]
    struct Recorder {
        keys: Vec<u32>,
        ticks: usize,
        pastes: Vec<String>,
    }

    impl Widget for Recorder {
        fn draw(&mut self, ctx: &DrawContext) -> Surface {
            Surface::with_size(ctx.max.size())
        }
        fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
            match event {
                Event::KeyPress(key) => {
                    self.keys.push(key.codepoint);
                    ctx.consume_and_redraw();
                }
                Event::Tick => {
                    self.ticks += 1;
                    ctx.consume_and_redraw();
                }
                Event::Paste(text) => {
                    self.pastes.push(text.clone());
                    ctx.consume_and_redraw();
                }
                _ => {}
            }
        }
        fn wants_events(&self) -> bool {
            true
        }
    }

    /// A root that replaces its focused child when it receives `swap`.
    struct FocusSwap {
        first: Rc<RefCell<Recorder>>,
        second: Rc<RefCell<Recorder>>,
        swapped: bool,
    }

    impl Widget for FocusSwap {
        fn draw(&mut self, ctx: &DrawContext) -> Surface {
            let child = if self.swapped {
                Rc::clone(&self.second)
            } else {
                Rc::clone(&self.first)
            };
            let mut surface = Surface::with_size(ctx.max.size());
            surface.children.push(SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface: draw_widget(&to_widget_ref(child), ctx),
                z_index: 0,
            });
            surface
        }

        fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
            if let Event::App(user) = event
                && user.name == "swap"
            {
                self.swapped = true;
                ctx.request_focus(to_widget_ref(Rc::clone(&self.second)));
                ctx.redraw = true;
            }
        }

        fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
            if let Event::Init = event {
                ctx.request_focus(to_widget_ref(Rc::clone(&self.first)));
            }
        }

        fn wants_events(&self) -> bool {
            true
        }
    }

    /// A root widget that paints one glyph into the top-left cell, so changing
    /// the glyph between renders yields a distinct frame and repainting the
    /// same glyph yields an unchanged one.
    struct Painter {
        glyph: char,
    }

    impl Widget for Painter {
        fn draw(&mut self, ctx: &DrawContext) -> Surface {
            let mut surface = Surface::with_size(ctx.max.size());
            surface.write_cell(
                0,
                0,
                Cell {
                    char: Character::new(self.glyph.to_string(), 1),
                    ..Cell::default()
                },
            );
            surface
        }
        fn wants_events(&self) -> bool {
            true
        }
    }

    /// Writes `bytes` to the pipe write end, panicking on short write.
    fn write_all(fd: &OwnedFd, bytes: &[u8]) {
        let n = nix::unistd::write(fd, bytes).expect("write to pipe");
        assert_eq!(n, bytes.len(), "short write to pipe");
    }

    /// Builds and initializes an `AsyncApp` over a `TestTty` and a pipe. The
    /// returned write end feeds the reader. Keep it alive or the reader sees
    /// EOF.
    async fn init_app() -> (AsyncApp, OwnedFd, Rc<RefCell<Recorder>>, WidgetRef) {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        // Answer the DA1 probe up front so init's detection wait returns as
        // soon as the reader consumes it, instead of after the 1s timeout.
        // The handshake latches, so consuming the reply before the probe
        // batch even goes out still unblocks the wait.
        write_all(&write_fd, b"\x1b[?c");

        let recorder = Rc::new(RefCell::new(Recorder::default()));
        let root: WidgetRef = to_widget_ref(Rc::clone(&recorder));
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            read_fd,
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        (app, write_fd, recorder, root)
    }

    #[tokio::test]
    async fn focus_change_refreshes_dispatch_path_before_render() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        write_all(&write_fd, b"\x1b[?c");
        let first = Rc::new(RefCell::new(Recorder::default()));
        let second = Rc::new(RefCell::new(Recorder::default()));
        let root: WidgetRef = to_widget_ref(Rc::new(RefCell::new(FocusSwap {
            first: Rc::clone(&first),
            second: Rc::clone(&second),
            swapped: false,
        })));
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            read_fd,
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");

        app.post_app_event(UserEvent {
            name: "swap".to_string(),
            data: None,
        });
        write_all(&write_fd, b"x");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);

        assert!(first.borrow().keys.is_empty(), "old child stayed inert");
        assert_eq!(second.borrow().keys, vec![u32::from('x')]);
    }

    #[tokio::test]
    async fn key_press_reaches_the_widget_and_latches_redraw() {
        let (mut app, write_fd, recorder, _root) = init_app().await;
        assert!(!app.needs_redraw(), "init's first draw clears the latch");

        write_all(&write_fd, b"j");
        let event = app.next_input().await.expect("input event");
        let frame = app.handle_input(event);

        assert!(!frame.quit);
        assert!(app.needs_redraw());
        assert_eq!(recorder.borrow().keys, vec![u32::from('j')]);
    }

    #[tokio::test]
    async fn bracketed_paste_reaches_the_widget_as_one_coalesced_event() {
        // The regression guard for the whole seam: raw bracketed-paste bytes go
        // in at the pipe, and the widget must see a single Paste carrying the
        // content, not a burst of per-character key presses.
        let (mut app, write_fd, recorder, _root) = init_app().await;

        write_all(&write_fd, b"\x1b[200~line one\nline two\x1b[201~");
        let event = app.next_input().await.expect("input event");
        app.handle_input(event);

        assert_eq!(
            recorder.borrow().pastes,
            vec!["line one\nline two".to_string()]
        );
        assert!(
            recorder.borrow().keys.is_empty(),
            "paste content must not leak as key presses"
        );
    }

    #[tokio::test]
    async fn winsize_event_resizes_the_screen_and_latches_redraw() {
        let (mut app, _write_fd, _recorder, _root) = init_app().await;

        let frame = app.handle_input(Event::Winsize(Winsize {
            rows: 50,
            cols: 100,
            x_pixel: 800,
            y_pixel: 1200,
        }));

        assert!(!frame.quit);
        assert!(app.needs_redraw());
        let screen = app.vaxis().screen.borrow();
        assert_eq!(screen.width, 100);
        assert_eq!(screen.height, 50);
    }

    #[tokio::test]
    async fn keystroke_log_records_path_and_consumption() {
        let (mut app, write_fd, _recorder, _root) = init_app().await;

        write_all(&write_fd, b"ab");
        for _ in 0..2 {
            let event = app.next_input().await.expect("input event");
            app.handle_input(event);
        }

        let log: Vec<_> = app.keystroke_log().collect();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].key.codepoint, u32::from('a'));
        assert_eq!(log[1].key.codepoint, u32::from('b'));
        for record in log {
            // The default debug label is the type name, which ends in the
            // concrete widget type.
            assert_eq!(record.path.len(), 1);
            assert!(record.path[0].ends_with("Recorder"));
            // The Recorder consumes key presses at-target.
            let (label, phase) = record.consumed_by.expect("key was consumed");
            assert!(label.ends_with("Recorder"));
            assert_eq!(phase, crate::vxfw::Phase::AtTarget);
        }
    }

    #[tokio::test]
    async fn due_timers_deliver_tick_to_the_widget() {
        let (mut app, _write_fd, recorder, root) = init_app().await;

        app.core.timers.push(Tick {
            deadline: Instant::now() - Duration::from_millis(1),
            widget: Rc::clone(&root),
        });
        assert!(app.next_tick_deadline().is_some());

        let frame = app.fire_due_timers();

        assert!(!frame.quit);
        assert!(app.needs_redraw());
        assert_eq!(recorder.borrow().ticks, 1);
        assert!(app.next_tick_deadline().is_none());
    }

    #[tokio::test]
    async fn frame_stats_track_renders_and_report_zero_cells_for_unchanged_frames() {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        // Answer the DA1 probe up front so init's detection wait returns
        // promptly instead of after the 1s timeout.
        write_all(&write_fd, b"\x1b[?c");

        let painter = Rc::new(RefCell::new(Painter { glyph: 'A' }));
        let root: WidgetRef = to_widget_ref(Rc::clone(&painter));
        let mut app = AsyncApp::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            read_fd,
        );
        app.init(Rc::clone(&root), Options::default())
            .await
            .expect("init");
        // Keep the write end alive so the reader does not see EOF.
        let _write_fd = write_fd;

        // init drew the first frame. A single frame has no meaningful rate.
        let stats = app.frame_stats();
        assert_eq!(stats.frames, 1, "init drew one frame");
        assert_eq!(stats.fps, 0.0, "a single frame reports zero fps");

        // Drive several more distinct frames by changing what the painter draws.
        let mut count = 1;
        for glyph in ['B', 'C', 'D'] {
            painter.borrow_mut().glyph = glyph;
            app.request_redraw();
            app.render(&root).expect("render");
            count += 1;
        }

        let stats = app.frame_stats();
        assert_eq!(stats.frames, count, "every render pushes one record");
        assert!(stats.frames <= FRAME_STATS_CAP);
        assert!(stats.avg > Duration::ZERO, "avg render time is non-zero");
        assert!(stats.max > Duration::ZERO, "max render time is non-zero");
        assert!(stats.last_cells > 0, "a changed frame emits cells");
        assert_eq!(stats.size, (40, 80), "TestTty reports 40 rows x 80 cols");
        assert!(stats.fps.is_finite() && stats.fps >= 0.0);

        // Render again without changing the painter: the diff emits nothing.
        app.render(&root).expect("render");
        let stats = app.frame_stats();
        assert_eq!(stats.frames, count + 1);
        assert_eq!(
            stats.last_cells, 0,
            "an unchanged frame reports zero changed cells"
        );
    }

    /// A 2x2 RGBA image encoded as PNG bytes for `Source::Mem`.
    fn tiny_png() -> Vec<u8> {
        let img = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2,
            2,
            image::Rgba([10, 20, 30, 255]),
        ));
        let mut png = Vec::new();
        img.write_with_encoder(image::codecs::png::PngEncoder::new(&mut png))
            .expect("encode png");
        png
    }

    /// Builds and initializes an `AsyncApp` over a `TestTty` and a pipe, with
    /// the kitty graphics capability set to `kitty_graphics`. Returns the write
    /// end so the caller keeps the reader from seeing EOF.
    async fn init_graphics_app(kitty_graphics: bool) -> (AsyncApp, OwnedFd) {
        let (read_fd, write_fd) = nix::unistd::pipe().expect("pipe");
        write_all(&write_fd, b"\x1b[?c");

        let root: WidgetRef = to_widget_ref(Rc::new(RefCell::new(Recorder::default())));
        // Seeding `caps` before `init` survives detection: `query_terminal_send`
        // seeds the shared detected state from the current caps, and the DA1
        // reply folds nothing that turns kitty graphics back off.
        let mut vx = Vaxis::new(VaxisOptions::default());
        vx.caps.kitty_graphics = kitty_graphics;
        let mut app = AsyncApp::new(vx, Box::new(TestTty::new()), read_fd);
        app.init(root, Options::default()).await.expect("init");
        (app, write_fd)
    }

    #[tokio::test]
    async fn load_image_transmits_when_graphics_capable() {
        let (mut app, _write_fd) = init_graphics_app(true).await;

        let img = app.load_image(Source::Mem(tiny_png())).expect("load image");

        // The capability gate passed, the first id was allocated, and the
        // delegation ran without a write error against the tty.
        assert_eq!(img.id(), 1);
        assert_eq!((img.width(), img.height()), (2, 2));
    }

    #[tokio::test]
    async fn load_image_errors_without_graphics_capability() {
        let (mut app, _write_fd) = init_graphics_app(false).await;

        assert!(matches!(
            app.load_image(Source::Mem(tiny_png())),
            Err(Error::NoGraphicsCapability)
        ));
    }
}
