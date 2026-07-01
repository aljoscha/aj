//! Port of upstream `examples/counter.zig`: the canonical vxfw tutorial.
//!
//! A `Counter` app widget owns a click count and a [`Button`]. On start (and on
//! every focus-in) it requests focus for the button so Enter and Ctrl-J click
//! it; the mouse clicks it too. Each click bumps the count and the button
//! relabels itself "Clicks: N". Ctrl-C quits.
//!
//! # Widget wiring
//!
//! Upstream pairs the button's `onClick` with a `userdata` pointer back to the
//! model. We drop that split: the count lives in an `Rc<Cell<u32>>` shared
//! between the button's `on_click` closure (which increments it) and the
//! `Counter` (which reads it to set the label). The button is held as a
//! concrete `Rc<RefCell<Button>>` so `draw` can relabel it, and it coerces to a
//! [`WidgetRef`] for focus requests and [`draw_widget`].

use std::cell::{Cell, RefCell};
use std::error::Error;
use std::rc::Rc;

use vaxis::key::Modifiers;
use vaxis::tty::PosixTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    App, Button, DrawContext, Event, EventContext, MaxSize, Options, RelativePoint, SubSurface,
    Surface, Widget, WidgetRef, draw_widget, to_widget_ref,
};

/// The application state: the click count and the button that drives it.
struct Counter {
    count: Rc<Cell<u32>>,
    button: Rc<RefCell<Button>>,
}

impl Widget for Counter {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max = ctx.max.size();

        // Relabel the button from the current count before drawing it.
        let count = self.count.get();
        self.button.borrow_mut().label = if count > 0 {
            format!("Clicks: {count}")
        } else {
            "Click me!".to_string()
        };

        // A Button expands to fill its area, so it needs a hard maximum. Give it
        // a fixed 16x3 box at the top-left corner of the screen.
        let button = to_widget_ref(Rc::clone(&self.button));
        let button_ctx = ctx.with_constraints(
            ctx.min,
            MaxSize {
                width: Some(16),
                height: Some(3),
            },
        );
        let button_surface = draw_widget(&button, &button_ctx);

        Surface {
            size: max,
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: vec![SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface: button_surface,
                z_index: 0,
            }],
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            // The root widget always gets an Init event first. We (and focus-in)
            // hand focus to the button so key events reach it.
            Event::Init | Event::FocusIn => {
                let button = to_widget_ref(Rc::clone(&self.button));
                ctx.request_focus(button);
            }
            Event::KeyPress(key) => {
                if key.matches(u32::from('c'), Modifiers::CTRL) {
                    ctx.quit = true;
                }
            }
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let count = Rc::new(Cell::new(0u32));

    // The click callback captures the shared count directly.
    let click_count = Rc::clone(&count);
    let button = Rc::new(RefCell::new(Button::new("Click me!", move |ctx| {
        click_count.set(click_count.get().saturating_add(1));
        ctx.consume_and_redraw();
    })));

    let model: WidgetRef = Rc::new(RefCell::new(Counter { count, button }));
    run(model)
}

/// Wires an [`App`] over the real terminal and runs `root`.
///
/// The `App` needs a writer-side tty and an independent read source. A
/// [`PosixTty`] is the writer; [`PosixTty::dup_reader`] hands back a dup'd
/// read handle over the same terminal for the input loop.
fn run(root: WidgetRef) -> Result<(), Box<dyn Error>> {
    let tty = PosixTty::new()?;
    let source = tty.dup_reader()?;
    let vx = Vaxis::new(VaxisOptions::default());
    let mut app = App::new(vx, Box::new(tty), Box::new(source));
    app.run(root, Options::default())?;
    Ok(())
}
