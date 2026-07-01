//! Port of upstream `examples/split_view.zig`: a [`SplitView`] with two [`Text`]
//! panes divided by a draggable separator.
//!
//! The split is constrained to a 10-column left pane. Dragging the separator
//! with the mouse resizes the panes. Ctrl-C quits.

use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;

use vaxis::key::Modifiers;
use vaxis::tty::PosixTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    App, DrawContext, Event, EventContext, Options, RelativePoint, SplitView, SubSurface, Surface,
    Text, Widget, WidgetRef, draw_widget, to_widget_ref,
};

/// The application state: the split and its two text panes.
struct SplitModel {
    split: Rc<RefCell<SplitView>>,
}

impl Widget for SplitModel {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max = ctx.max.size();
        let split = to_widget_ref(Rc::clone(&self.split));
        let surface = draw_widget(&split, ctx);
        Surface {
            size: max,
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: vec![SubSurface {
                origin: RelativePoint { row: 0, col: 0 },
                surface,
                z_index: 0,
            }],
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if let Event::KeyPress(key) = event {
            if key.matches(u32::from('c'), Modifiers::CTRL) {
                ctx.quit = true;
            }
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let lhs: WidgetRef = Rc::new(RefCell::new(Text::new("Left hand side")));
    let rhs: WidgetRef = Rc::new(RefCell::new(Text::new("right hand side")));
    let split = Rc::new(RefCell::new(SplitView::new(lhs, rhs, 10)));

    let model: WidgetRef = Rc::new(RefCell::new(SplitModel { split }));
    run(model)
}

/// Wires an [`App`] over the real terminal and runs `root`. See `counter.rs`.
fn run(root: WidgetRef) -> Result<(), Box<dyn Error>> {
    let tty = PosixTty::new()?;
    let source = tty.dup_reader()?;
    let vx = Vaxis::new(VaxisOptions::default());
    let mut app = App::new(vx, Box::new(tty), Box::new(source));
    app.run(root, Options::default())?;
    Ok(())
}
