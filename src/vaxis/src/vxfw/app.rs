//! The `App` runtime: the frame loop, focus and mouse handling, and tick
//! scheduling.
//!
//! `App` owns the [`Vaxis`] runtime, the [`Tty`] writer side, and a read
//! [`ByteSource`] it hands to the threaded [`Loop`] on [`run`](App::run). The
//! frame loop paces to a deadline, fires due timers, drains input events through
//! the focus and mouse handlers, lays out the widget tree, and renders.
//!
//! # Event vs frame state
//!
//! The App resets the per-event state ([`EventContext::consume_event`] and the
//! [`Phase`]) after each event, but the `redraw` latch is per-frame: it persists
//! across all of a frame's events and timers and is cleared only when the App
//! draws. So a handler consuming one event does not leak that to the next, while
//! any redraw request survives until the frame is drawn.
//!
//! # Cross-frame survival (D3)
//!
//! Mouse hit-testing runs against the *previous* frame's surface, so
//! [`MouseHandler`] owns `last_frame`. Because we own the `Surface` tree with
//! plain `Vec`s, keeping it alive into the next frame is just holding the value.
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

use crate::Winsize;
use crate::cell::{self, CursorShape};
use crate::error::Error;
use crate::event::Event as InternalEvent;
use crate::event_loop::{ByteSource, FromEvent, Loop, WinsizeSource};
use crate::key::Key;
use crate::mouse::Mouse;
use crate::tty::Tty;
use crate::vaxis::Vaxis;
use crate::vxfw::{
    Command, DrawContext, Event, EventContext, HitResult, MaxSize, Phase, Point, Size, Surface,
    Tick, WidgetRef, draw_widget, widget_eq,
};
use crate::window::ChildOptions;

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

/// The widget-framework application: a [`Vaxis`] runtime, a [`Tty`] writer, a
/// read source for the threaded loop, the tick schedule, and a pending focus
/// request.
pub struct App {
    vx: Vaxis,
    tty: Box<dyn Tty>,
    /// The read side, moved into the [`Loop`] on [`run`](App::run).
    source: Option<Box<dyn ByteSource>>,
    /// Pending timers, kept sorted by [`Tick::cmp_by_deadline_desc`] so the
    /// soonest is last and [`check_timers`](App::check_timers) pops it first.
    timers: Vec<Tick>,
    /// A focus request from a handler, applied before the next layout.
    wants_focus: Option<WidgetRef>,
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
            vx,
            tty,
            source: Some(source),
            timers: Vec::new(),
            wants_focus: None,
        }
    }

    /// Runs the application until a handler sets [`EventContext::quit`].
    pub fn run(&mut self, root: WidgetRef, opts: Options) -> Result<(), Error> {
        // Size the screen from the tty before the first layout so the cell-size
        // division below has a non-zero denominator.
        let initial_ws = self.tty.get_winsize()?;
        self.vx.resize(&mut self.tty.writer(), initial_ws)?;

        let source: Box<dyn ByteSource> = match self.source.take() {
            Some(source) => source,
            None => Box::new(io::empty()),
        };
        let mut input_loop: Loop<LoopEvent> = Loop::new(source, self.vx.shared());
        // NOTE: A fixed-snapshot winsize source. A real backend supplies a live
        // ioctl-backed source so resizes are observed; the in-memory test
        // backend has a fixed size, for which the snapshot is exact.
        let winsize: WinsizeSource = Arc::new(move || Ok(initial_ws));
        input_loop.set_winsize_source(winsize);

        input_loop.start();
        // Always start the app with an init event and a focus event.
        input_loop.post_event(LoopEvent::Init);
        input_loop.post_event(LoopEvent::FocusIn);

        self.vx.enter_alt_screen(&mut self.tty.writer())?;
        self.vx
            .query_terminal(&mut self.tty.writer(), Duration::from_secs(1))?;
        self.vx.set_bracketed_paste(&mut self.tty.writer(), true)?;
        self.vx
            .subscribe_to_color_scheme_updates(&mut self.tty.writer())?;

        // Only run the out-of-band SIGWINCH path when the terminal does not
        // report resizes in-band. We wait until detection finished (above) to
        // decide.
        let use_signal_resize = !self.vx.shared().in_band_resize();
        if use_signal_resize {
            input_loop.install_resize_handler(self.tty.as_ref())?;
        }

        // We do not use pixel mouse, so force it off before enabling mouse mode.
        self.vx.caps.sgr_pixels = false;
        self.vx.set_mouse_mode(&mut self.tty.writer(), true)?;

        let framerate: u64 = if opts.framerate > 0 {
            u64::from(opts.framerate)
        } else {
            60
        };
        let tick = Duration::from_nanos(1_000_000_000 / framerate);

        let result = self.frame_loop(&input_loop, &root, tick);

        if use_signal_resize {
            input_loop.uninstall_resize_handler(self.tty.as_ref());
        }
        input_loop.stop();
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

            self.check_timers(&mut ctx);

            while let Some(loop_event) = input_loop.try_event() {
                let event = loop_event.into_event();
                match &event {
                    Event::Mouse(mouse) => mouse_handler.handle_mouse(self, &mut ctx, *mouse),
                    Event::FocusOut => {
                        mouse_handler.mouse_exit(self, &mut ctx);
                        focus_handler.handle_event(&mut ctx, &event);
                        self.handle_command(&mut ctx.cmds);
                    }
                    Event::Winsize(ws) => {
                        self.vx.resize(&mut self.tty.writer(), *ws)?;
                        ctx.redraw = true;
                    }
                    _ => {
                        focus_handler.handle_event(&mut ctx, &event);
                        self.handle_command(&mut ctx.cmds);
                    }
                }
                // Per-event reset (defer semantics): clears consume_event and
                // the phase between events but leaves the per-frame redraw latch.
                reset_event_state(&mut ctx);
            }

            // Handle a focus change before we lay out.
            if let Some(widget) = self.wants_focus.take() {
                focus_handler.focus_widget(&mut ctx, widget);
                self.handle_command(&mut ctx.cmds);
            }

            if ctx.quit {
                return Ok(());
            }
            if !ctx.redraw {
                continue;
            }
            ctx.redraw = false;
            debug_assert!(ctx.cmds.is_empty());

            let mut surface = self.do_layout(root);
            // Updating the mouse against the fresh surface may change hover
            // state and request another redraw.
            mouse_handler.update_mouse(self, &surface, &mut ctx);
            if let Some(widget) = self.wants_focus.take() {
                focus_handler.focus_widget(&mut ctx, widget);
                self.handle_command(&mut ctx.cmds);
            }
            debug_assert!(ctx.cmds.is_empty());
            if ctx.redraw {
                surface = self.do_layout(root);
            }

            mouse_handler.last_frame = surface;
            focus_handler.update(&mouse_handler.last_frame);
            let focused = Rc::clone(&focus_handler.focused);
            self.render(&mouse_handler.last_frame, &focused)?;
        }
    }

    /// Lays out `widget` as the root, constrained to the full screen.
    fn do_layout(&self, widget: &WidgetRef) -> Surface {
        let (width, height, width_pix, height_pix, width_method) = {
            let screen = self.vx.screen.borrow();
            (
                screen.width,
                screen.height,
                screen.width_pix,
                screen.height_pix,
                screen.width_method,
            )
        };
        // Guard the per-cell pixel division: a zero-sized screen would divide by
        // zero. The App resizes before the first layout, so this only bites
        // degenerate (0x0) screens.
        let cell_size = Size {
            width: if width == 0 { 0 } else { width_pix / width },
            height: if height == 0 { 0 } else { height_pix / height },
        };
        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(width),
                height: Some(height),
            },
            cell_size,
            width_method,
        };
        draw_widget(widget, &ctx)
    }

    /// Clears the screen, blits `surface`, and diff-renders to the tty.
    fn render(&mut self, surface: &Surface, focused: &WidgetRef) -> Result<(), Error> {
        {
            let win = self.vx.window();
            win.clear();
            win.hide_cursor();
            win.set_cursor_shape(CursorShape::Default);
            let root_win = win.child(ChildOptions {
                width: Some(surface.size.width),
                height: Some(surface.size.height),
                ..ChildOptions::default()
            });
            surface.render(root_win, Some(focused));
            // `win` borrows `self.vx`; drop it here before the `&mut self.vx`
            // render call below.
        }
        self.vx.render(&mut self.tty.writer())?;
        Ok(())
    }

    /// Adds `tick`, keeping `timers` sorted so the soonest deadline is last.
    fn add_tick(&mut self, tick: Tick) {
        self.timers.push(tick);
        self.timers.sort_by(Tick::cmp_by_deadline_desc);
    }

    /// Applies every queued command, draining the queue.
    ///
    /// Render failures from the byte-emitting commands are dropped: those
    /// commands (clipboard, title, notify, color query) are best-effort and
    /// upstream logs and continues.
    fn handle_command(&mut self, cmds: &mut Vec<Command>) {
        for cmd in cmds.drain(..) {
            match cmd {
                Command::Tick(tick) => self.add_tick(tick),
                Command::SetMouseShape(shape) => self.vx.set_mouse_shape(shape),
                Command::RequestFocus(widget) => self.wants_focus = Some(widget),
                Command::CopyToClipboard(content) => {
                    let _ = self
                        .vx
                        .copy_to_system_clipboard(&mut self.tty.writer(), &content);
                }
                Command::SetTitle(title) => {
                    let _ = self.vx.set_title(&mut self.tty.writer(), &title);
                }
                Command::QueueRefresh => self.vx.queue_refresh(),
                Command::Notify { title, body } => {
                    let _ = self
                        .vx
                        .notify(&mut self.tty.writer(), title.as_deref(), &body);
                }
                Command::QueryColor(kind) => {
                    let _ = self.vx.query_color(&mut self.tty.writer(), kind);
                }
            }
        }
    }

    /// Fires every timer whose deadline has passed, delivering [`Event::Tick`].
    ///
    /// `timers` is sorted descending, so we pop the soonest deadline first and
    /// stop at the first not-yet-due tick (re-adding it). The per-tick state is
    /// reset before and after delivery so a consuming tick does not leak its
    /// consumption.
    fn check_timers(&mut self, ctx: &mut EventContext) {
        let now = Instant::now();
        while let Some(tick) = self.timers.pop() {
            if tick.deadline > now {
                // Not yet due. It is the soonest, so re-adding keeps the order.
                self.timers.push(tick);
                break;
            }
            reset_event_state(ctx);
            ctx.phase = Phase::AtTarget;
            dispatch_event(&tick.widget, ctx, &Event::Tick);
            reset_event_state(ctx);
        }
        self.handle_command(&mut ctx.cmds);
    }
}

/// Resets the per-event state: clears `consume_event` and returns to the
/// capturing phase. Leaves the per-frame `redraw` latch untouched.
fn reset_event_state(ctx: &mut EventContext) {
    ctx.consume_event = false;
    ctx.phase = Phase::Capturing;
}

/// Delivers `event` to `w` during the at-target or bubbling phase.
fn dispatch_event(w: &WidgetRef, ctx: &mut EventContext, event: &Event) {
    w.borrow_mut().handle_event(ctx, event);
}

/// Delivers `event` to `w` during the capturing phase.
fn dispatch_capture(w: &WidgetRef, ctx: &mut EventContext, event: &Event) {
    w.borrow_mut().capture_event(ctx, event);
}

/// Builds a mouse event with the position translated into a widget's local
/// coordinates.
fn local_mouse_event(mouse: Mouse, local: Point) -> Event {
    let mut m = mouse;
    m.col = i16::try_from(local.col).unwrap_or(i16::MAX);
    m.row = i16::try_from(local.row).unwrap_or(i16::MAX);
    Event::Mouse(m)
}

/// Diffs two hit lists to deliver enter/leave events.
///
/// Widgets in `old` but not `new` get [`Event::MouseLeave`]; widgets in `new`
/// but not `old` get [`Event::MouseEnter`]; widgets in both get nothing.
fn diff_hit_lists(old: &[HitResult], new: &[HitResult], app: &mut App, ctx: &mut EventContext) {
    for a in old {
        if !new.iter().any(|b| widget_eq(&a.widget, &b.widget)) {
            dispatch_event(&a.widget, ctx, &Event::MouseLeave);
            app.handle_command(&mut ctx.cmds);
        }
    }
    for b in new {
        if !old.iter().any(|a| widget_eq(&b.widget, &a.widget)) {
            dispatch_event(&b.widget, ctx, &Event::MouseEnter);
            app.handle_command(&mut ctx.cmds);
        }
    }
}

/// Tracks the mouse across frames: the last drawn surface (for hit-testing),
/// the last hit list (for enter/leave diffing), and the last mouse position.
struct MouseHandler {
    last_frame: Surface,
    last_hit_list: Vec<HitResult>,
    mouse: Option<Mouse>,
}

impl MouseHandler {
    fn init(root: WidgetRef) -> MouseHandler {
        MouseHandler {
            last_frame: Surface {
                size: Size::default(),
                widget: Some(root),
                cursor: None,
                buffer: Vec::new(),
                children: Vec::new(),
            },
            last_hit_list: Vec::new(),
            mouse: None,
        }
    }

    /// Dispatches a mouse event: hit-test the last frame, diff for enter/leave,
    /// then walk capture (root to target-exclusive), target, and bubble
    /// (target-exclusive back to root), stopping on consume.
    fn handle_mouse(&mut self, app: &mut App, ctx: &mut EventContext, mouse: Mouse) {
        self.mouse = Some(mouse);

        let mut hits: Vec<HitResult> = Vec::new();
        if let Some(point) = surface_point(&self.last_frame, mouse) {
            self.last_frame.hit_test(point, &mut hits);
        }

        diff_hit_lists(&self.last_hit_list, &hits, app, ctx);
        self.last_hit_list = hits.clone();

        // The deepest hit is the target; the rest are ancestors root-first.
        let Some(target) = hits.pop() else {
            return;
        };

        ctx.phase = Phase::Capturing;
        for item in &hits {
            let event = local_mouse_event(mouse, item.local);
            dispatch_capture(&item.widget, ctx, &event);
            app.handle_command(&mut ctx.cmds);
            if ctx.consume_event {
                return;
            }
        }

        ctx.phase = Phase::AtTarget;
        {
            let event = local_mouse_event(mouse, target.local);
            dispatch_event(&target.widget, ctx, &event);
            app.handle_command(&mut ctx.cmds);
            if ctx.consume_event {
                return;
            }
        }

        ctx.phase = Phase::Bubbling;
        while let Some(item) = hits.pop() {
            let event = local_mouse_event(mouse, item.local);
            dispatch_event(&item.widget, ctx, &event);
            app.handle_command(&mut ctx.cmds);
            if ctx.consume_event {
                return;
            }
        }
    }

    /// Refreshes hover state against the freshly drawn `surface`, delivering
    /// enter/leave events (no capture/target/bubble walk).
    fn update_mouse(&mut self, app: &mut App, surface: &Surface, ctx: &mut EventContext) {
        let Some(mouse) = self.mouse else {
            return;
        };
        let mut hits: Vec<HitResult> = Vec::new();
        if let Some(point) = surface_point(surface, mouse) {
            surface.hit_test(point, &mut hits);
        }
        diff_hit_lists(&self.last_hit_list, &hits, app, ctx);
        self.last_hit_list = hits;
    }

    /// Sends [`Event::MouseLeave`] to every widget in the last hit list, used
    /// when the window loses focus.
    fn mouse_exit(&self, app: &mut App, ctx: &mut EventContext) {
        for item in &self.last_hit_list {
            dispatch_event(&item.widget, ctx, &Event::MouseLeave);
            app.handle_command(&mut ctx.cmds);
        }
    }
}

/// Translates a mouse report into a surface-local [`Point`], or `None` if it
/// falls outside the surface. Negative coordinates are never inside.
fn surface_point(surface: &Surface, mouse: Mouse) -> Option<Point> {
    let row = u16::try_from(mouse.row).ok()?;
    let col = u16::try_from(mouse.col).ok()?;
    if col < surface.size.width && row < surface.size.height {
        Some(Point { row, col })
    } else {
        None
    }
}

/// Maintains the path from the root to the focused widget and delivers focus
/// events along it (capture down, at-target, bubble up).
struct FocusHandler {
    root: WidgetRef,
    focused: WidgetRef,
    /// Root-first path to the focused widget, rebuilt each frame by
    /// [`update`](FocusHandler::update).
    path_to_focused: Vec<WidgetRef>,
}

impl FocusHandler {
    fn init(root: WidgetRef) -> FocusHandler {
        FocusHandler {
            focused: Rc::clone(&root),
            root,
            path_to_focused: Vec::new(),
        }
    }

    /// Rebuilds the focus path from `surface`. If the focused widget is not in
    /// the tree, the path falls back to the root.
    fn update(&mut self, surface: &Surface) {
        self.path_to_focused.clear();
        // Builds the path focused-first by appending on the way back up the
        // recursion, then reverses to root-first below.
        self.child_has_focus(surface);

        let root_is_surface = surface
            .widget
            .as_ref()
            .is_some_and(|w| widget_eq(&self.root, w));
        if !root_is_surface {
            // The surface root is not our initial widget, so append it.
            self.path_to_focused.push(Rc::clone(&self.root));
        }

        self.path_to_focused.reverse();
    }

    /// Whether `surface` or one of its descendants is the focused widget,
    /// appending each ancestor to the path on the way up.
    fn child_has_focus(&mut self, surface: &Surface) -> bool {
        if let Some(w) = &surface.widget {
            if widget_eq(&self.focused, w) {
                self.path_to_focused.push(Rc::clone(w));
                return true;
            }
        }
        for child in &surface.children {
            if self.child_has_focus(&child.surface) {
                if let Some(w) = &surface.widget {
                    self.path_to_focused.push(Rc::clone(w));
                }
                return true;
            }
        }
        false
    }

    /// Moves focus to `widget`, sending focus-out to the old focus and focus-in
    /// to the new. Asserts the target wants events.
    fn focus_widget(&mut self, ctx: &mut EventContext, widget: WidgetRef) {
        debug_assert!(
            widget.borrow().wants_events(),
            "a focusable widget must want events"
        );
        if widget_eq(&self.focused, &widget) {
            return;
        }
        ctx.phase = Phase::AtTarget;
        dispatch_event(&self.focused, ctx, &Event::FocusOut);
        self.focused = widget;
        dispatch_event(&self.focused, ctx, &Event::FocusIn);
    }

    /// Delivers `event` along the focus path: capture root-to-target, at-target,
    /// then bubble target-exclusive back to root. Each phase stops on consume.
    fn handle_event(&self, ctx: &mut EventContext, event: &Event) {
        debug_assert!(!self.path_to_focused.is_empty());

        ctx.phase = Phase::Capturing;
        for widget in &self.path_to_focused {
            dispatch_capture(widget, ctx, event);
            if ctx.consume_event {
                return;
            }
        }

        ctx.phase = Phase::AtTarget;
        let target = self
            .path_to_focused
            .last()
            .expect("focus path is non-empty");
        dispatch_event(target, ctx, event);
        if ctx.consume_event {
            return;
        }

        ctx.phase = Phase::Bubbling;
        let target_idx = self.path_to_focused.len() - 1;
        for widget in self.path_to_focused[..target_idx].iter().rev() {
            dispatch_event(widget, ctx, event);
            if ctx.consume_event {
                return;
            }
        }
    }
}

/// The `Send` event type carried by the App's threaded [`Loop`].
///
/// NOTE: This is the reader-produced subset of [`Event`] plus [`LoopEvent::Init`]
/// (which the App posts on start). It exists because the full vxfw [`Event`]
/// holds an `Rc` in its `App` variant and so is not `Send`, while the loop's
/// reader thread requires a `Send` event. The App converts each drained
/// `LoopEvent` into an [`Event`] for dispatch, so application-posted
/// `Event::App` values do not travel through this loop.
enum LoopEvent {
    KeyPress(Key),
    KeyRelease(Key),
    Mouse(Mouse),
    MouseLeave,
    FocusIn,
    FocusOut,
    PasteStart,
    PasteEnd,
    Paste(String),
    ColorReport(cell::Report),
    ColorScheme(cell::Scheme),
    Winsize(Winsize),
    Init,
}

impl LoopEvent {
    /// Converts a loop event into the user-facing dispatch event.
    fn into_event(self) -> Event {
        match self {
            LoopEvent::KeyPress(k) => Event::KeyPress(k),
            LoopEvent::KeyRelease(k) => Event::KeyRelease(k),
            LoopEvent::Mouse(m) => Event::Mouse(m),
            LoopEvent::MouseLeave => Event::MouseLeave,
            LoopEvent::FocusIn => Event::FocusIn,
            LoopEvent::FocusOut => Event::FocusOut,
            LoopEvent::PasteStart => Event::PasteStart,
            LoopEvent::PasteEnd => Event::PasteEnd,
            LoopEvent::Paste(s) => Event::Paste(s),
            LoopEvent::ColorReport(r) => Event::ColorReport(r),
            LoopEvent::ColorScheme(s) => Event::ColorScheme(s),
            LoopEvent::Winsize(ws) => Event::Winsize(ws),
            LoopEvent::Init => Event::Init,
        }
    }
}

impl FromEvent for LoopEvent {
    fn from_event(event: InternalEvent) -> Option<Self> {
        Some(match event {
            InternalEvent::KeyPress(k) => LoopEvent::KeyPress(k),
            InternalEvent::KeyRelease(k) => LoopEvent::KeyRelease(k),
            InternalEvent::Mouse(m) => LoopEvent::Mouse(m),
            InternalEvent::MouseLeave => LoopEvent::MouseLeave,
            InternalEvent::FocusIn => LoopEvent::FocusIn,
            InternalEvent::FocusOut => LoopEvent::FocusOut,
            InternalEvent::PasteStart => LoopEvent::PasteStart,
            InternalEvent::PasteEnd => LoopEvent::PasteEnd,
            InternalEvent::Paste(s) => LoopEvent::Paste(s),
            InternalEvent::ColorReport(r) => LoopEvent::ColorReport(r),
            InternalEvent::ColorScheme(s) => LoopEvent::ColorScheme(s),
            InternalEvent::Winsize(ws) => LoopEvent::Winsize(ws),
            // Capability responses are consumed by the reader, never delivered.
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use super::App;
    use crate::tty::TestTty;
    use crate::vaxis::{Options as VaxisOptions, Vaxis};
    use crate::vxfw::{DrawContext, Event, EventContext, Phase, Surface, Tick, Widget, WidgetRef};

    #[test]
    fn timer_consume_does_not_leak_to_the_next_event() {
        // A widget that consumes (and requests a redraw on) tick events.
        struct TestWidget;
        impl Widget for TestWidget {
            fn draw(&mut self, _ctx: &DrawContext) -> Surface {
                unreachable!("draw is not exercised by this test")
            }
            fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
                if matches!(event, Event::Tick) {
                    ctx.consume_and_redraw();
                }
            }
            fn wants_events(&self) -> bool {
                true
            }
        }

        let widget: WidgetRef = Rc::new(RefCell::new(TestWidget));
        let mut app = App::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
            Box::new(std::io::empty()),
        );

        // A timer already past its deadline fires immediately.
        let now = Instant::now();
        app.timers.push(Tick {
            deadline: now - Duration::from_millis(1),
            widget: Rc::clone(&widget),
        });

        let mut ctx = EventContext::new();
        app.check_timers(&mut ctx);

        // The tick set redraw, but the per-event reset cleared consume_event and
        // the phase, so the consumption does not leak to the next event.
        assert!(ctx.redraw);
        assert!(!ctx.consume_event);
        assert_eq!(ctx.phase, Phase::Capturing);
    }
}
