//! Select-only mouse input for a list row with caller-owned rendering and identity.

use crate::mouse::{Button, Type};
use crate::vxfw::{DrawContext, Event, EventContext, Surface, Widget, WidgetRef};

/// A left press selects the rendered row without confirming or taking focus.
/// The callback returns whether selection succeeded. It should reject a stale
/// row when its source has changed since the last paint. A successful selection
/// consumes the press and requests a redraw.
pub struct SelectableRow {
    content: WidgetRef,
    select: Box<dyn FnMut() -> bool>,
}

impl SelectableRow {
    pub fn new(content: WidgetRef, select: impl FnMut() -> bool + 'static) -> Self {
        Self {
            content,
            select: Box::new(select),
        }
    }
}

impl Widget for SelectableRow {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        self.content.borrow_mut().draw(ctx)
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        if let Event::Mouse(mouse) = event
            && mouse.button == Button::Left
            && mouse.kind == Type::Press
            && (self.select)()
        {
            ctx.consume_and_redraw();
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    #[test]
    fn selectable_row() {
        let valid = Rc::new(Cell::new(false));
        let accepts = Rc::clone(&valid);
        let mut row = SelectableRow::new(
            Rc::new(RefCell::new(crate::vxfw::Text::new("row"))),
            move || accepts.get(),
        );
        let event = |button, kind| {
            Event::Mouse(crate::mouse::Mouse {
                row: 0,
                col: 0,
                xoffset: 0,
                yoffset: 0,
                mods: Default::default(),
                button,
                kind,
            })
        };
        let mut ctx = EventContext::new();
        row.handle_event(&mut ctx, &event(Button::Left, Type::Press));
        assert!(!ctx.consume_event && !ctx.redraw, "a stale row is ignored");
        valid.set(true);
        for (button, kind) in [
            (Button::Right, Type::Press),
            (Button::Left, Type::Release),
            (Button::Left, Type::Drag),
        ] {
            row.handle_event(&mut ctx, &event(button, kind));
            assert!(!ctx.consume_event && !ctx.redraw);
        }
        row.handle_event(&mut ctx, &event(Button::Left, Type::Press));
        assert!(ctx.consume_event && ctx.redraw);
    }
}
