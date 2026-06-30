//! [`Spinner`]: a tick-driven braille spinner with a thread-safe run counter.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

use crate::cell::{Cell, Character, Style};
use crate::vxfw::{
    Command, DrawContext, Event, EventContext, Size, Surface, Tick, Widget, WidgetRef,
};

/// The braille frames the spinner cycles through.
const FRAMES: [&str; 8] = ["⣶", "⣧", "⣏", "⡟", "⠿", "⢻", "⣹", "⣼"];
/// Milliseconds between frames (12 fps).
const TIME_LAPSE: u32 = 1000 / 12;

/// A braille spinner driven by [`Event::Tick`].
///
/// [`start`](Spinner::start) and [`stop`](Spinner::stop) maintain a thread-safe
/// run counter: the spinner animates while the count is positive. The frame
/// advances on each tick, which re-arms the next tick. When the count drops to
/// zero the `was_spinning` latch lets the spinner draw one final clearing frame
/// before going idle.
///
/// NOTE: Upstream stores a `std.Io` handle only to compute a tick deadline.
/// Our [`Tick`] takes its deadline from `Instant::now` plus a millisecond
/// offset, so there is no `io` field. The deadline comes from
/// [`EventContext::tick`] / [`Tick::in_ms`] instead.
pub struct Spinner {
    /// A weak self-reference so the spinner can schedule ticks targeting
    /// itself. A widget cannot otherwise name its own `WidgetRef` from inside a
    /// method, so we capture it at construction with [`Rc::new_cyclic`].
    me: Weak<RefCell<Spinner>>,
    count: AtomicU16,
    pub style: Style,
    /// The current frame index into [`FRAMES`].
    frame: u8,
    /// Stays true from `start` until a tick observes a zero count, so the
    /// spinner draws one more frame to clear itself after stopping.
    was_spinning: AtomicBool,
}

impl Spinner {
    /// Creates a spinner behind a `WidgetRef`.
    pub fn new() -> Rc<RefCell<Spinner>> {
        Rc::new_cyclic(|me| {
            RefCell::new(Spinner {
                me: Weak::clone(me),
                count: AtomicU16::new(0),
                style: Style::default(),
                frame: 0,
                was_spinning: AtomicBool::new(false),
            })
        })
    }

    /// Increments the run counter, returning a first [`Command::Tick`] only on
    /// the 0 to 1 transition. Thread safe.
    pub fn start(&self) -> Option<Command> {
        self.was_spinning.store(true, Ordering::Relaxed);
        let count = self.count.fetch_add(1, Ordering::Relaxed);
        if count == 0 {
            Some(Tick::in_ms(TIME_LAPSE, self.widget()))
        } else {
            None
        }
    }

    /// Decrements the run counter, saturating at zero. The spinner stops once
    /// the counter reaches zero. Thread safe.
    pub fn stop(&self) {
        let count = self.count.load(Ordering::Relaxed);
        self.count.store(count.saturating_sub(1), Ordering::Relaxed);
    }

    /// The spinner's own `WidgetRef`, used to target self-scheduled ticks.
    fn widget(&self) -> WidgetRef {
        self.me.upgrade().expect("spinner self-reference is live")
    }
}

impl Widget for Spinner {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let size = Size {
            width: ctx.min.width.max(1),
            height: ctx.min.height.max(1),
        };
        let mut surface = Surface::with_size(size);
        let base = Cell {
            style: self.style,
            ..Cell::default()
        };
        for cell in &mut surface.buffer {
            *cell = base.clone();
        }

        if self.count.load(Ordering::Relaxed) == 0 {
            return surface;
        }

        surface.write_cell(
            0,
            0,
            Cell {
                char: Character::new(FRAMES[usize::from(self.frame)], 1),
                style: self.style,
                ..Cell::default()
            },
        );
        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if !matches!(event, Event::Tick) {
            return;
        }
        let count = self.count.load(Ordering::Relaxed);
        if count == 0 {
            // The spinner has stopped. Draw one more clearing frame the first
            // time we see the zero count, then go idle.
            if self.was_spinning.load(Ordering::Relaxed) {
                ctx.redraw = true;
                self.was_spinning.store(false, Ordering::Relaxed);
            }
            return;
        }

        self.frame += 1;
        if usize::from(self.frame) >= FRAMES.len() {
            self.frame = 0;
        }

        ctx.tick(TIME_LAPSE, self.widget());
    }

    fn wants_events(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gwidth;
    use crate::vxfw::{MaxSize, draw_widget};

    #[test]
    fn spinner() {
        let spinner = Spinner::new();

        // Starting from a zero count returns a first tick command.
        let maybe_cmd = spinner.borrow().start();
        assert!(matches!(maybe_cmd, Some(Command::Tick(_))));
        assert_eq!(spinner.borrow().count.load(Ordering::Relaxed), 1);

        // Starting again only bumps the counter, no new command.
        let maybe_cmd = spinner.borrow().start();
        assert!(maybe_cmd.is_none());
        assert_eq!(spinner.borrow().count.load(Ordering::Relaxed), 2);

        // Delivering a tick advances the frame and re-arms.
        let mut ctx = EventContext::new();
        spinner.borrow_mut().handle_event(&mut ctx, &Event::Tick);
        assert_eq!(spinner.borrow().frame, 1);

        // The spinner draws at 1x1 by default.
        let cloned = Rc::clone(&spinner);
        let widget: WidgetRef = cloned;
        let surface = draw_widget(
            &widget,
            &DrawContext {
                min: Size {
                    width: 0,
                    height: 0,
                },
                max: MaxSize {
                    width: None,
                    height: None,
                },
                cell_size: Size {
                    width: 10,
                    height: 20,
                },
                width_method: gwidth::Method::Unicode,
            },
        );
        assert_eq!(surface.size.width, 1);
        assert_eq!(surface.size.height, 1);

        // Stopping decrements the counter back to zero.
        spinner.borrow().stop();
        assert_eq!(spinner.borrow().count.load(Ordering::Relaxed), 1);
        spinner.borrow().stop();
        assert_eq!(spinner.borrow().count.load(Ordering::Relaxed), 0);
    }
}
