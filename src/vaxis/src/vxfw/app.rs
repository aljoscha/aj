//! The synchronous `App` runtime: a threaded input loop and a paced frame loop
//! over the shared vxfw engine.
//!
//! `App` owns an [`AppCore`] (the [`Vaxis`](crate::vaxis::Vaxis) runtime, the
//! [`Tty`] writer side, the tick schedule) and a read [`ByteSource`] it hands to
//! the threaded [`Loop`] on [`run`](App::run). The frame loop paces to a
//! deadline, fires due timers, drains input events through the focus and mouse
//! handlers, lays out the widget tree, and renders.
//!
//! # Event vs frame state
//!
//! The App resets the per-event state ([`EventContext::consume_event`] and the
//! [`Phase`](crate::vxfw::Phase)) after each event, but the `redraw` latch is
//! per-frame: it persists across all of a frame's events and timers and is
//! cleared only when the App draws. So a handler consuming one event does not
//! leak that to the next, while any redraw request survives until the frame is
//! drawn.
//!
//! # The threaded loop and the event types
//!
//! The loop carries [`LoopEvent`], a `Send` type, because the full vxfw
//! [`Event`] holds an `Rc` (in `Event::App`) and is not `Send`. The App converts
//! each drained `LoopEvent` into an `Event` for dispatch and synthesizes the
//! rest (`Tick`, `Init`, the mouse enter/leave pair).

use std::io;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::error::Error;
use crate::event_loop::{ByteSource, Loop, WinsizeSource};
use crate::tty::Tty;
use crate::vaxis::Vaxis;
use crate::vxfw::app_core::{AppCore, FocusHandler, MouseHandler, reset_event_state};
use crate::vxfw::loop_event::LoopEvent;
use crate::vxfw::{Event, EventContext, WidgetRef};

/// Runtime options for [`App::run`].
pub struct Options {
    /// Frames per second. Zero falls back to 60.
    pub framerate: u8,
}

impl Default for Options {
    fn default() -> Self {
        Self { framerate: 60 }
    }
}

/// The widget-framework application: the shared engine plus a read source for
/// the threaded loop.
pub struct App {
    core: AppCore,
    /// The read side, moved into the [`Loop`] on [`run`](App::run).
    source: Option<Box<dyn ByteSource>>,
}

impl App {
    /// Creates an app over `vx` (the runtime and writer-side `tty`) and a `source`
    /// the loop reads input from.
    ///
    /// The read and write sides are separate objects: a real backend hands a
    /// dup'd tty fd (or a second open of `/dev/tty`) as `source` while keeping
    /// the writer in `tty`.
    pub fn new(vx: Vaxis, tty: Box<dyn Tty>, source: Box<dyn ByteSource>) -> App {
        App {
            core: AppCore {
                vx,
                tty,
                timers: Vec::new(),
                wants_focus: None,
            },
            source: Some(source),
        }
    }

    /// Runs the application until a handler sets [`EventContext::quit`].
    pub fn run(&mut self, root: WidgetRef, opts: Options) -> Result<(), Error> {
        // Size the screen from the tty before the first layout so the cell-size
        // division below has a non-zero denominator.
        let initial_ws = self.core.tty.get_winsize()?;
        self.core
            .vx
            .resize(&mut self.core.tty.writer(), initial_ws)?;

        let source: Box<dyn ByteSource> = match self.source.take() {
            Some(source) => source,
            None => Box::new(io::empty()),
        };
        let mut input_loop: Loop<LoopEvent> = Loop::new(source, self.core.vx.shared());
        // NOTE: A fixed-snapshot winsize source. A real backend supplies a live
        // ioctl-backed source so resizes are observed; the in-memory test
        // backend has a fixed size, for which the snapshot is exact.
        let winsize: WinsizeSource = Arc::new(move || Ok(initial_ws));
        input_loop.set_winsize_source(winsize);

        input_loop.start();
        // Always start the app with an init event and a focus event.
        input_loop.post_event(LoopEvent::Init);
        input_loop.post_event(LoopEvent::FocusIn);

        self.core.vx.enter_alt_screen(&mut self.core.tty.writer())?;
        self.core
            .vx
            .query_terminal(&mut self.core.tty.writer(), Duration::from_secs(1))?;
        self.core
            .vx
            .set_bracketed_paste(&mut self.core.tty.writer(), true)?;
        self.core
            .vx
            .subscribe_to_color_scheme_updates(&mut self.core.tty.writer())?;

        // Only run the out-of-band SIGWINCH path when the terminal does not
        // report resizes in-band. We wait until detection finished (above) to
        // decide.
        let use_signal_resize = !self.core.vx.shared().in_band_resize();
        if use_signal_resize {
            input_loop.install_resize_handler(self.core.tty.as_ref())?;
        }

        // We do not use pixel mouse, so force it off before enabling mouse mode.
        self.core.vx.caps.sgr_pixels = false;
        self.core
            .vx
            .set_mouse_mode(&mut self.core.tty.writer(), true)?;

        let framerate: u64 = if opts.framerate > 0 {
            u64::from(opts.framerate)
        } else {
            60
        };
        let tick = Duration::from_nanos(1_000_000_000 / framerate);

        let result = self.frame_loop(&input_loop, &root, tick);

        if use_signal_resize {
            input_loop.uninstall_resize_handler(self.core.tty.as_ref());
        }
        // Signal the reader to quit, then provoke a byte so its blocking read
        // wakes. The reader parks in a blocking `read` on the tty and only
        // re-checks the quit flag once a byte arrives, so without the wake the
        // `stop` join below would block until the user's next keypress. A
        // device-status report round-trips through the terminal to deliver that
        // byte. Signalling before writing keeps this race-free (see
        // `Loop::signal_stop`), and we own the writer here so we can drive it.
        input_loop.signal_stop();
        let _ = self
            .core
            .vx
            .device_status_report(&mut self.core.tty.writer());
        input_loop.stop();

        // Restore the terminal on the way out: show the cursor, leave the alt
        // screen, and disable mouse and bracketed paste. Upstream does this in
        // `app.deinit()`. Best-effort so we do not mask the loop's result, and
        // we flush because the writer is buffered and the reset bytes must reach
        // the terminal before the app returns (e.g. before a caller prints to
        // stdout).
        let _ = self.core.vx.reset_state(&mut self.core.tty.writer());
        let _ = self.core.tty.writer().flush();

        result
    }

    /// The per-frame loop, factored out so it can use `?` while
    /// [`run`](App::run) still tears the loop down on the way out.
    fn frame_loop(
        &mut self,
        input_loop: &Loop<LoopEvent>,
        root: &WidgetRef,
        tick: Duration,
    ) -> Result<(), Error> {
        let mut mouse_handler = MouseHandler::init(Rc::clone(root));
        let mut focus_handler = FocusHandler::init(Rc::clone(root));
        focus_handler.path_to_focused.push(Rc::clone(root));

        let mut next_frame = Instant::now();
        let mut ctx = EventContext::new();

        loop {
            let now = Instant::now();
            if now >= next_frame {
                // Deadline exceeded; schedule the next frame without sleeping.
                next_frame = now + tick;
            } else {
                std::thread::sleep(next_frame - now);
                next_frame += tick;
            }

            self.core.check_timers(&mut ctx);

            while let Some(loop_event) = input_loop.try_event() {
                let event = loop_event.into_event();
                match &event {
                    Event::Mouse(mouse) => {
                        mouse_handler.handle_mouse(&mut self.core, &mut ctx, *mouse)
                    }
                    Event::FocusOut => {
                        mouse_handler.mouse_exit(&mut self.core, &mut ctx);
                        focus_handler.handle_event(&mut ctx, &event);
                        self.core.handle_command(&mut ctx.cmds);
                    }
                    Event::Winsize(ws) => {
                        self.core.vx.resize(&mut self.core.tty.writer(), *ws)?;
                        ctx.redraw = true;
                    }
                    _ => {
                        focus_handler.handle_event(&mut ctx, &event);
                        self.core.handle_command(&mut ctx.cmds);
                    }
                }
                // Per-event reset (defer semantics): clears consume_event and
                // the phase between events but leaves the per-frame redraw latch.
                reset_event_state(&mut ctx);
            }

            // Handle a focus change before we lay out.
            if let Some(widget) = self.core.wants_focus.take() {
                focus_handler.focus_widget(&mut ctx, widget);
                self.core.handle_command(&mut ctx.cmds);
            }

            if ctx.quit {
                return Ok(());
            }
            if !ctx.redraw {
                continue;
            }
            ctx.redraw = false;
            debug_assert!(ctx.cmds.is_empty());

            let mut surface = self.core.do_layout(root);
            // Updating the mouse against the fresh surface may change hover
            // state and request another redraw.
            mouse_handler.update_mouse(&mut self.core, &surface, &mut ctx);
            if let Some(widget) = self.core.wants_focus.take() {
                focus_handler.focus_widget(&mut ctx, widget);
                self.core.handle_command(&mut ctx.cmds);
            }
            debug_assert!(ctx.cmds.is_empty());
            if ctx.redraw {
                surface = self.core.do_layout(root);
            }

            mouse_handler.last_frame = surface;
            focus_handler.update(&mouse_handler.last_frame);
            let focused = Rc::clone(&focus_handler.focused);
            self.core.render(&mouse_handler.last_frame, &focused)?;
        }
    }
}
