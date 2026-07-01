//! Port of upstream `examples/list_view.zig`: a [`ListView`] over 80 [`Text`]
//! rows with a movable cursor.
//!
//! The list is focused on every event so j/k, the arrow keys, and the mouse
//! wheel all reach it. `q` or Ctrl-C quits. The list is drawn inset one row and
//! one column from the top-left.

use std::cell::RefCell;
use std::error::Error;
use std::rc::Rc;

use vaxis::key::Modifiers;
use vaxis::tty::PosixTty;
use vaxis::vaxis::{Options as VaxisOptions, Vaxis};
use vaxis::vxfw::{
    App, DrawContext, Event, EventContext, ListView, Options, RelativePoint, Source, SubSurface,
    Surface, Text, Widget, WidgetRef, draw_widget, to_widget_ref,
};

/// The application state: just the list view.
struct ListModel {
    list_view: Rc<RefCell<ListView>>,
}

impl Widget for ListModel {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max = ctx.max.size();
        let list_view = to_widget_ref(Rc::clone(&self.list_view));
        let surface = draw_widget(&list_view, ctx);
        Surface {
            size: max,
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children: vec![SubSurface {
                origin: RelativePoint { row: 1, col: 1 },
                surface,
                z_index: 0,
            }],
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        // Keep focus on the list so its own handler receives navigation keys and
        // the wheel; the quit keys bubble back up to us.
        let list_view = to_widget_ref(Rc::clone(&self.list_view));
        ctx.request_focus(list_view);
        if let Event::KeyPress(key) = event {
            if key.matches(u32::from('q'), Modifiers::empty())
                || key.match_exact(u32::from('c'), Modifiers::CTRL)
            {
                ctx.quit = true;
            }
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let n: usize = 80;
    let mut texts: Vec<WidgetRef> = Vec::with_capacity(n);
    for i in 0..n {
        let text: WidgetRef = Rc::new(RefCell::new(Text::new(format!("List Item {i} of {n}"))));
        texts.push(text);
    }

    let mut list_view = ListView::new(Source::Slice(texts));
    list_view.wheel_scroll = 3;

    let model: WidgetRef = Rc::new(RefCell::new(ListModel {
        list_view: Rc::new(RefCell::new(list_view)),
    }));
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
