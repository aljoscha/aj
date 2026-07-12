//! The loop-agnostic vxfw engine shared by the runtimes.
//!
//! [`AppCore`] owns the terminal (the [`Vaxis`] runtime and the [`Tty`] writer),
//! the tick schedule, and the pending focus request. The dispatch machinery
//! around it ([`MouseHandler`], [`FocusHandler`], and the free dispatch helpers)
//! implements the capture/target/bubble walks over the widget tree. The
//! synchronous `App` and the async `AsyncApp` both drive this engine, each with
//! its own loop style.
//!
//! # Cross-frame survival (D3)
//!
//! Mouse hit-testing runs against the *previous* frame's surface, so
//! [`MouseHandler`] owns `last_frame`. Because we own the `Surface` tree with
//! plain `Vec`s, keeping it alive into the next frame is just holding the value.

use std::collections::VecDeque;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::cell::CursorShape;
use crate::error::Error;
use crate::key::Key;
use crate::mouse::Mouse;
use crate::tty::Tty;
use crate::vaxis::Vaxis;
use crate::vxfw::render_debug;
use crate::vxfw::{
    Command, DrawContext, Event, EventContext, HitResult, MaxSize, Phase, Point, Size, Surface,
    Tick, WidgetRef, draw_widget, widget_eq,
};
use crate::window::ChildOptions;

/// The engine state a runtime drives: the [`Vaxis`] runtime, the [`Tty`]
/// writer, the tick schedule, and a pending focus request.
pub(crate) struct AppCore {
    pub(crate) vx: Vaxis,
    pub(crate) tty: Box<dyn Tty>,
    /// Pending timers, kept sorted by [`Tick::cmp_by_deadline_desc`] so the
    /// soonest is last and [`check_timers`](AppCore::check_timers) pops it
    /// first.
    pub(crate) timers: Vec<Tick>,
    /// A focus request from a handler, applied before the next layout.
    pub(crate) wants_focus: Option<WidgetRef>,
    /// Recent frame records, newest at the back, capped at [`FRAME_STATS_CAP`].
    frames: VecDeque<FrameRecord>,
    /// Layout time accumulated by [`do_layout`](AppCore::do_layout) during the
    /// current frame. [`render`](AppCore::render) takes it and folds it into
    /// the frame record, then it resets to zero for the next frame.
    pending_layout: Duration,
}

/// A snapshot of recent frame-render statistics, for a host debug overlay.
///
/// The profiler behind it runs always-on (it is cheap: two `Instant` reads and
/// a bounded push per frame). The values describe the frames before the one
/// being drawn, so an overlay that reads this while drawing is always one frame
/// behind, which is correct: the current frame's cost is not known until its
/// render completes.
#[derive(Debug, Clone, Copy)]
pub struct FrameStats {
    /// Wall time of the most recent frame's render call.
    pub last: Duration,
    /// Mean render time over the retained window.
    pub avg: Duration,
    /// Worst render time over the retained window. This is the number the
    /// overlay exists to surface: a single slow frame is an input-latency hitch.
    pub max: Duration,
    /// Frames retained in the window (<= capacity).
    pub frames: usize,
    /// Render rate over the retained window: frames divided by the wall span
    /// between the oldest and newest retained frame. This is a true redraw
    /// rate, not a fixed cadence. Because it is computed from two fixed record
    /// timestamps, when idle (no new frames) it stays frozen at that last
    /// window rather than decaying.
    pub fps: f32,
    /// Cells the last frame's diff actually emitted. In a diff-rendered TUI this
    /// is the true repaint cost: a small scroll that re-lays-out a large block
    /// shows up here even when `last` looks cheap.
    pub last_cells: u32,
    /// Screen size (rows, cols) at the last frame (rows * cols is the
    /// worst-case repaint).
    pub size: (u16, u16),
}

/// One entry in the frame-stats ring buffer.
struct FrameRecord {
    /// When the frame's render call completed.
    end: Instant,
    /// Wall time of the layout + blit + diff + write span.
    draw: Duration,
    /// Cells the frame's diff emitted.
    cells: u32,
    /// Screen size (rows, cols) at the frame.
    size: (u16, u16),
}

/// How many frame records the profiler retains (~4s at 60fps).
///
/// Exposed to the crate so the async runtime's tests can name the cap.
pub(crate) const FRAME_STATS_CAP: usize = 240;

impl AppCore {
    /// Builds the engine over `vx` (the runtime and writer-side `tty`), with no
    /// pending timers, focus request, or frame history.
    pub(crate) fn new(vx: Vaxis, tty: Box<dyn Tty>) -> AppCore {
        AppCore {
            vx,
            tty,
            timers: Vec::new(),
            wants_focus: None,
            frames: VecDeque::new(),
            pending_layout: Duration::ZERO,
        }
    }

    /// Lays out `widget` as the root, constrained to the full screen.
    ///
    /// NOTE: We time the layout here and fold it into `pending_layout` rather
    /// than in the callers. Every caller is a render sequence and the frame's
    /// initial and re-layout calls are both real layout work for the same
    /// frame, so accumulating here attributes the whole span to the frame
    /// without leaking into the next one (`render` takes it).
    pub(crate) fn do_layout(&mut self, widget: &WidgetRef) -> Surface {
        let start = Instant::now();
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
        // zero. The runtime resizes before the first layout, so this only bites
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
        let surface = draw_widget(widget, &ctx);
        self.pending_layout += start.elapsed();
        surface
    }

    /// Clears the screen, blits `surface`, and diff-renders to the tty.
    ///
    /// This is the shared point both runtimes hit, so the frame profiler
    /// records here: the timed span is layout + blit + diff + write (the
    /// layout part arrives via `pending_layout`, mouse and focus work is
    /// excluded), and the record also carries the diff's changed-cell count
    /// and the screen size.
    pub(crate) fn render(&mut self, surface: &Surface, focused: &WidgetRef) -> Result<(), Error> {
        // Time the blit + diff + write span. `do_layout` already folded this
        // frame's layout time into `pending_layout`.
        let start = Instant::now();
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
        let render_result = self.vx.render(&mut self.tty.writer());
        // Take `pending_layout` before the `?` so a failed render does not
        // leak this frame's layout time into the next frame.
        let draw = std::mem::take(&mut self.pending_layout) + start.elapsed();
        render_result?;

        let size = {
            let screen = self.vx.screen.borrow();
            (screen.height, screen.width)
        };
        self.push_frame(FrameRecord {
            end: Instant::now(),
            draw,
            cells: self.vx.last_render_cells(),
            size,
        });

        if render_debug::enabled() {
            render_debug::inspect_frame(surface, &self.vx.screen.borrow(), &self.vx.screen_last);
        }
        Ok(())
    }

    /// Pushes `rec` onto the frame-stats ring buffer, evicting the oldest
    /// record first when the buffer is at [`FRAME_STATS_CAP`].
    fn push_frame(&mut self, rec: FrameRecord) {
        if self.frames.len() == FRAME_STATS_CAP {
            self.frames.pop_front();
        }
        self.frames.push_back(rec);
    }

    /// A snapshot of recent frame-render statistics for a host debug overlay.
    ///
    /// See [`compute_frame_stats`] for the snapshot contract.
    pub(crate) fn frame_stats(&self) -> FrameStats {
        compute_frame_stats(&self.frames)
    }

    /// Adds `tick`, keeping `timers` sorted so the soonest deadline is last.
    pub(crate) fn add_tick(&mut self, tick: Tick) {
        self.timers.push(tick);
        self.timers.sort_by(Tick::cmp_by_deadline_desc);
    }

    /// Applies every queued command, draining the queue.
    ///
    /// Render failures from the byte-emitting commands are dropped: those
    /// commands (clipboard, title, notify, color query) are best-effort and
    /// upstream logs and continues.
    pub(crate) fn handle_command(&mut self, cmds: &mut Vec<Command>) {
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
    pub(crate) fn check_timers(&mut self, ctx: &mut EventContext) {
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

/// Computes a [`FrameStats`] snapshot from a window of frame records.
///
/// `last`, `last_cells`, and `size` come from the newest record (zeros and
/// `(0, 0)` when the window is empty). `avg` and `max` span the retained
/// window. `fps` is frames over the wall span between the oldest and newest
/// record, and is `0.0` with fewer than two frames or a zero span.
fn compute_frame_stats(frames: &VecDeque<FrameRecord>) -> FrameStats {
    let count = frames.len();
    let Some(newest) = frames.back() else {
        return FrameStats {
            last: Duration::ZERO,
            avg: Duration::ZERO,
            max: Duration::ZERO,
            frames: 0,
            fps: 0.0,
            last_cells: 0,
            size: (0, 0),
        };
    };

    let sum: Duration = frames.iter().map(|f| f.draw).sum();
    // `count >= 1` here (newest exists) and is bounded by the cap, so both
    // conversions are lossless and the mean divides safely.
    let avg = sum / u32::try_from(count).expect("frame count fits u32");
    let max = frames
        .iter()
        .map(|f| f.draw)
        .max()
        .expect("non-empty window");

    // Guard the division: fewer than two frames or a zero span (both records
    // share an instant) has no meaningful rate, so report 0.0.
    let oldest = frames.front().expect("non-empty window");
    let span = newest.end.duration_since(oldest.end).as_secs_f32();
    let fps = if count < 2 || span == 0.0 {
        0.0
    } else {
        f32::from(u16::try_from(count).expect("frame count fits u16")) / span
    };

    FrameStats {
        last: newest.draw,
        avg,
        max,
        frames: count,
        fps,
        last_cells: newest.cells,
        size: newest.size,
    }
}

/// Resets the per-event state: clears `consume_event` and returns to the
/// capturing phase. Leaves the per-frame `redraw` latch untouched.
pub(crate) fn reset_event_state(ctx: &mut EventContext) {
    ctx.consume_event = false;
    ctx.phase = Phase::Capturing;
}

/// Delivers `event` to `w` during the at-target or bubbling phase.
pub(crate) fn dispatch_event(w: &WidgetRef, ctx: &mut EventContext, event: &Event) {
    w.borrow_mut().handle_event(ctx, event);
}

/// Delivers `event` to `w` during the capturing phase.
pub(crate) fn dispatch_capture(w: &WidgetRef, ctx: &mut EventContext, event: &Event) {
    w.borrow_mut().capture_event(ctx, event);
}

/// Builds a mouse event with the position translated into a widget's local
/// coordinates.
pub(crate) fn local_mouse_event(mouse: Mouse, local: Point) -> Event {
    let mut m = mouse;
    m.col = i16::try_from(local.col).unwrap_or(i16::MAX);
    m.row = i16::try_from(local.row).unwrap_or(i16::MAX);
    Event::Mouse(m)
}

/// Diffs two hit lists to deliver enter/leave events.
///
/// Widgets in `old` but not `new` get [`Event::MouseLeave`]; widgets in `new`
/// but not `old` get [`Event::MouseEnter`]; widgets in both get nothing.
pub(crate) fn diff_hit_lists(
    old: &[HitResult],
    new: &[HitResult],
    core: &mut AppCore,
    ctx: &mut EventContext,
) {
    for a in old {
        if !new.iter().any(|b| widget_eq(&a.widget, &b.widget)) {
            dispatch_event(&a.widget, ctx, &Event::MouseLeave);
            core.handle_command(&mut ctx.cmds);
        }
    }
    for b in new {
        if !old.iter().any(|a| widget_eq(&b.widget, &a.widget)) {
            dispatch_event(&b.widget, ctx, &Event::MouseEnter);
            core.handle_command(&mut ctx.cmds);
        }
    }
}

/// Tracks the mouse across frames: the last drawn surface (for hit-testing),
/// the last hit list (for enter/leave diffing), and the last mouse position.
pub(crate) struct MouseHandler {
    pub(crate) last_frame: Surface,
    pub(crate) last_hit_list: Vec<HitResult>,
    pub(crate) mouse: Option<Mouse>,
}

impl MouseHandler {
    pub(crate) fn init(root: WidgetRef) -> MouseHandler {
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
    pub(crate) fn handle_mouse(
        &mut self,
        core: &mut AppCore,
        ctx: &mut EventContext,
        mouse: Mouse,
    ) {
        self.mouse = Some(mouse);

        let mut hits: Vec<HitResult> = Vec::new();
        if let Some(point) = surface_point(&self.last_frame, mouse) {
            self.last_frame.hit_test(point, &mut hits);
        }

        diff_hit_lists(&self.last_hit_list, &hits, core, ctx);
        self.last_hit_list = hits.clone();

        // The deepest hit is the target; the rest are ancestors root-first.
        let Some(target) = hits.pop() else {
            return;
        };

        ctx.phase = Phase::Capturing;
        for item in &hits {
            let event = local_mouse_event(mouse, item.local);
            dispatch_capture(&item.widget, ctx, &event);
            core.handle_command(&mut ctx.cmds);
            if ctx.consume_event {
                return;
            }
        }

        ctx.phase = Phase::AtTarget;
        {
            let event = local_mouse_event(mouse, target.local);
            dispatch_event(&target.widget, ctx, &event);
            core.handle_command(&mut ctx.cmds);
            if ctx.consume_event {
                return;
            }
        }

        ctx.phase = Phase::Bubbling;
        while let Some(item) = hits.pop() {
            let event = local_mouse_event(mouse, item.local);
            dispatch_event(&item.widget, ctx, &event);
            core.handle_command(&mut ctx.cmds);
            if ctx.consume_event {
                return;
            }
        }
    }

    /// Refreshes hover state against the freshly drawn `surface`, delivering
    /// enter/leave events (no capture/target/bubble walk).
    pub(crate) fn update_mouse(
        &mut self,
        core: &mut AppCore,
        surface: &Surface,
        ctx: &mut EventContext,
    ) {
        let Some(mouse) = self.mouse else {
            return;
        };
        let mut hits: Vec<HitResult> = Vec::new();
        if let Some(point) = surface_point(surface, mouse) {
            surface.hit_test(point, &mut hits);
        }
        diff_hit_lists(&self.last_hit_list, &hits, core, ctx);
        self.last_hit_list = hits;
    }

    /// Sends [`Event::MouseLeave`] to every widget in the last hit list, used
    /// when the window loses focus.
    pub(crate) fn mouse_exit(&self, core: &mut AppCore, ctx: &mut EventContext) {
        for item in &self.last_hit_list {
            dispatch_event(&item.widget, ctx, &Event::MouseLeave);
            core.handle_command(&mut ctx.cmds);
        }
    }
}

/// Translates a mouse report into a surface-local [`Point`], or `None` if it
/// falls outside the surface. Negative coordinates are never inside.
pub(crate) fn surface_point(surface: &Surface, mouse: Mouse) -> Option<Point> {
    let row = u16::try_from(mouse.row).ok()?;
    let col = u16::try_from(mouse.col).ok()?;
    if col < surface.size.width && row < surface.size.height {
        Some(Point { row, col })
    } else {
        None
    }
}

/// One entry in the keystroke log: which key went where, and who ate it.
#[derive(Debug, Clone)]
pub struct KeystrokeRecord {
    /// The dispatched key press.
    pub key: Key,
    /// Root-first debug labels of the focus path the key walked.
    pub path: Vec<&'static str>,
    /// The debug label of the widget that consumed the key and the phase it
    /// consumed it in, or `None` when the key fell through unconsumed.
    pub consumed_by: Option<(&'static str, Phase)>,
}

/// How many keystroke records the focus handler retains.
const KEYSTROKE_LOG_CAP: usize = 100;

/// Maintains the path from the root to the focused widget and delivers focus
/// events along it (capture down, at-target, bubble up).
pub(crate) struct FocusHandler {
    pub(crate) root: WidgetRef,
    pub(crate) focused: WidgetRef,
    /// Root-first path to the focused widget, rebuilt each frame by
    /// [`update`](FocusHandler::update).
    pub(crate) path_to_focused: Vec<WidgetRef>,
    /// The dispatch-debug log of recent key presses, oldest first.
    ///
    /// TODO: a focus-inspector overlay that renders the focus tree and this
    /// log with per-node handled markers. The record also still lacks the
    /// "which controller action fired" slot the keymap spec calls for, which
    /// needs a label channel from `KeymapController::fire` through the
    /// `EventContext`. Both belong to the inspector work.
    pub(crate) keystroke_log: VecDeque<KeystrokeRecord>,
}

impl FocusHandler {
    pub(crate) fn init(root: WidgetRef) -> FocusHandler {
        FocusHandler {
            focused: Rc::clone(&root),
            root,
            path_to_focused: Vec::new(),
            keystroke_log: VecDeque::new(),
        }
    }

    /// Rebuilds the focus path from `surface`. If the focused widget is not in
    /// the tree, the path falls back to the root.
    pub(crate) fn update(&mut self, surface: &Surface) {
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

        // The focused widget can vanish from the tree (a host that pops a
        // modal without moving focus, say). Fall back to a root-only path so
        // the next key still dispatches to the root instead of panicking on
        // an empty path in `handle_event`.
        if self.path_to_focused.is_empty() {
            self.path_to_focused.push(Rc::clone(&self.root));
        }
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
    pub(crate) fn focus_widget(&mut self, ctx: &mut EventContext, widget: WidgetRef) {
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
    ///
    /// Key presses are additionally recorded in the keystroke log with the
    /// path they walked and where (if anywhere) they were consumed.
    pub(crate) fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        debug_assert!(!self.path_to_focused.is_empty());

        let Event::KeyPress(key) = event else {
            self.dispatch_along_path(ctx, event);
            return;
        };

        // Collect the labels before dispatch: label borrows are transient, so
        // they cannot be held across the mutable dispatch borrows below.
        let path = self
            .path_to_focused
            .iter()
            .map(|w| w.borrow().debug_label())
            .collect();
        let consumed_by = self.dispatch_along_path(ctx, event);
        if self.keystroke_log.len() == KEYSTROKE_LOG_CAP {
            self.keystroke_log.pop_front();
        }
        self.keystroke_log.push_back(KeystrokeRecord {
            key: key.clone(),
            path,
            consumed_by,
        });
    }

    /// The three-phase walk itself, returning the label and phase of the
    /// consuming widget, if any.
    fn dispatch_along_path(
        &self,
        ctx: &mut EventContext,
        event: &Event,
    ) -> Option<(&'static str, Phase)> {
        ctx.phase = Phase::Capturing;
        for widget in &self.path_to_focused {
            dispatch_capture(widget, ctx, event);
            if ctx.consume_event {
                return Some((widget.borrow().debug_label(), Phase::Capturing));
            }
        }

        ctx.phase = Phase::AtTarget;
        let target = self
            .path_to_focused
            .last()
            .expect("focus path is non-empty");
        dispatch_event(target, ctx, event);
        if ctx.consume_event {
            return Some((target.borrow().debug_label(), Phase::AtTarget));
        }

        ctx.phase = Phase::Bubbling;
        let target_idx = self.path_to_focused.len() - 1;
        for widget in self.path_to_focused[..target_idx].iter().rev() {
            dispatch_event(widget, ctx, event);
            if ctx.consume_event {
                return Some((widget.borrow().debug_label(), Phase::Bubbling));
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;
    use std::time::{Duration, Instant};

    use super::{AppCore, FRAME_STATS_CAP, FocusHandler, FrameRecord, compute_frame_stats};
    use crate::tty::TestTty;
    use crate::vaxis::{Options as VaxisOptions, Vaxis};
    use crate::vxfw::{
        DrawContext, Event, EventContext, MaxSize, Phase, Size, Surface, Tick, Widget, WidgetRef,
        draw_widget, widget_eq,
    };

    /// A minimal engine over a `TestTty` for the frame-stats tests.
    fn test_core() -> AppCore {
        AppCore::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
        )
    }

    /// A frame record `off` after `base`, drawing for `draw` with `cells`
    /// changed at a fixed size, for the pure snapshot-math tests.
    fn rec(base: Instant, off: Duration, draw: Duration, cells: u32) -> FrameRecord {
        FrameRecord {
            end: base + off,
            draw,
            cells,
            size: (24, 80),
        }
    }

    #[test]
    fn push_frame_evicts_the_oldest_when_at_capacity() {
        let mut core = test_core();
        let base = Instant::now();
        // Push three past capacity, tagging each with its push index via
        // `cells` so we can name which records survived.
        for i in 0..(FRAME_STATS_CAP + 3) {
            core.push_frame(rec(
                base,
                Duration::from_millis(u64::try_from(i).unwrap()),
                Duration::from_millis(1),
                u32::try_from(i).unwrap(),
            ));
        }
        assert_eq!(core.frames.len(), FRAME_STATS_CAP);
        // The oldest three (indices 0, 1, 2) were evicted, so the front is the
        // fourth-pushed and the back is the last-pushed.
        assert_eq!(core.frames.front().unwrap().cells, 3);
        assert_eq!(
            core.frames.back().unwrap().cells,
            u32::try_from(FRAME_STATS_CAP + 2).unwrap()
        );
    }

    #[test]
    fn compute_frame_stats_reports_exact_window_values() {
        let base = Instant::now();
        // Three frames at t = 0, 10, 30 ms (span 30 ms), draws 2/4/6 ms.
        let mut frames = VecDeque::new();
        frames.push_back(rec(base, Duration::ZERO, Duration::from_millis(2), 100));
        frames.push_back(rec(
            base,
            Duration::from_millis(10),
            Duration::from_millis(4),
            101,
        ));
        frames.push_back(rec(
            base,
            Duration::from_millis(30),
            Duration::from_millis(6),
            102,
        ));

        let stats = compute_frame_stats(&frames);
        assert_eq!(stats.frames, 3);
        // Mean of 2/4/6 ms.
        assert_eq!(stats.avg, Duration::from_millis(4));
        assert_eq!(stats.max, Duration::from_millis(6));
        // `last`, `last_cells`, `size` come from the newest record.
        assert_eq!(stats.last, Duration::from_millis(6));
        assert_eq!(stats.last_cells, 102);
        assert_eq!(stats.size, (24, 80));
        // fps = frames / span = 3 / 0.030 s = 100.0, and must be nonzero for a
        // multi-frame window with a real span.
        assert!(stats.fps > 0.0);
        assert!((stats.fps - 100.0).abs() < 1e-3, "fps was {}", stats.fps);
    }

    #[test]
    fn compute_frame_stats_reflects_only_the_retained_window() {
        let mut core = test_core();
        let base = Instant::now();
        // Fill the window with cheap frames, then overwrite it entirely with
        // expensive ones. Every cheap record is evicted, so avg/max reflect
        // only the retained window rather than an all-time blend.
        let mut off = 0u64;
        for _ in 0..FRAME_STATS_CAP {
            core.push_frame(rec(
                base,
                Duration::from_millis(off),
                Duration::from_millis(1),
                0,
            ));
            off += 1;
        }
        for _ in 0..FRAME_STATS_CAP {
            core.push_frame(rec(
                base,
                Duration::from_millis(off),
                Duration::from_millis(100),
                0,
            ));
            off += 1;
        }

        let stats = compute_frame_stats(&core.frames);
        assert_eq!(stats.frames, FRAME_STATS_CAP);
        assert_eq!(
            stats.avg,
            Duration::from_millis(100),
            "evicted cheap frames must not drag the average"
        );
        assert_eq!(stats.max, Duration::from_millis(100));
    }

    #[test]
    fn compute_frame_stats_handles_empty_and_degenerate_windows() {
        // Empty window: all-zero stats, size (0, 0), no panic.
        let empty: VecDeque<FrameRecord> = VecDeque::new();
        let stats = compute_frame_stats(&empty);
        assert_eq!(stats.frames, 0);
        assert_eq!(stats.last, Duration::ZERO);
        assert_eq!(stats.avg, Duration::ZERO);
        assert_eq!(stats.max, Duration::ZERO);
        assert_eq!(stats.fps, 0.0);
        assert_eq!(stats.last_cells, 0);
        assert_eq!(stats.size, (0, 0));

        let base = Instant::now();

        // One frame: no span, so no rate.
        let mut one = VecDeque::new();
        one.push_back(rec(base, Duration::ZERO, Duration::from_millis(5), 7));
        let stats = compute_frame_stats(&one);
        assert_eq!(stats.frames, 1);
        assert_eq!(stats.fps, 0.0, "a single frame has no rate");

        // Two frames sharing an instant: zero span, so no rate and no inf/NaN.
        let mut equal = VecDeque::new();
        equal.push_back(rec(base, Duration::ZERO, Duration::from_millis(5), 7));
        equal.push_back(rec(base, Duration::ZERO, Duration::from_millis(5), 7));
        let stats = compute_frame_stats(&equal);
        assert_eq!(stats.frames, 2);
        assert!(stats.fps.is_finite());
        assert_eq!(stats.fps, 0.0, "a zero span has no rate");
    }

    #[test]
    fn render_resets_pending_layout() {
        struct Blank;
        impl Widget for Blank {
            fn draw(&mut self, ctx: &DrawContext) -> Surface {
                Surface::with_size(ctx.max.size())
            }
        }

        let mut core = test_core();
        // Seed a non-zero layout accumulation so the reset is observable: a
        // fresh frame must not inherit the previous frame's layout time.
        core.pending_layout = Duration::from_millis(7);

        let root: WidgetRef = Rc::new(RefCell::new(Blank));
        let surface = Surface::with_size(Size {
            width: 0,
            height: 0,
        });
        core.render(&surface, &root).expect("render");

        assert_eq!(core.pending_layout, Duration::ZERO);
    }

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
        let mut core = AppCore::new(
            Vaxis::new(VaxisOptions::default()),
            Box::new(TestTty::new()),
        );

        // A timer already past its deadline fires immediately.
        let now = Instant::now();
        core.timers.push(Tick {
            deadline: now - Duration::from_millis(1),
            widget: Rc::clone(&widget),
        });

        let mut ctx = EventContext::new();
        core.check_timers(&mut ctx);

        // The tick set redraw, but the per-event reset cleared consume_event and
        // the phase, so the consumption does not leak to the next event.
        assert!(ctx.redraw);
        assert!(!ctx.consume_event);
        assert_eq!(ctx.phase, Phase::Capturing);
    }

    #[test]
    fn focus_path_falls_back_to_root_when_the_focused_widget_vanishes() {
        struct Blank;
        impl Widget for Blank {
            fn draw(&mut self, ctx: &DrawContext) -> Surface {
                Surface::with_size(ctx.max.size())
            }
            fn wants_events(&self) -> bool {
                true
            }
        }

        let root: WidgetRef = Rc::new(RefCell::new(Blank));
        let vanished: WidgetRef = Rc::new(RefCell::new(Blank));
        let mut focus = FocusHandler::init(Rc::clone(&root));
        focus.focused = vanished;

        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(10),
                height: Some(4),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: crate::gwidth::Method::Unicode,
        };
        // The focused widget never drew into the tree, so the rebuilt path
        // must degrade to a root-only path rather than an empty one, which
        // would panic on the next dispatch.
        focus.update(&draw_widget(&root, &ctx));
        assert_eq!(focus.path_to_focused.len(), 1);
        assert!(widget_eq(&focus.path_to_focused[0], &root));

        let mut ev_ctx = EventContext::new();
        focus.handle_event(&mut ev_ctx, &Event::Tick);
    }
}
