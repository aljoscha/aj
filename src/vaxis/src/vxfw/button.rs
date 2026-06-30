//! [`Button`]: an interactive, focusable button that composes a centered label
//! and invokes a callback on click.

use std::cell::RefCell;
use std::rc::Rc;

use crate::cell::{Cell, Color, Style};
use crate::key::{Key, Modifiers};
use crate::mouse;
use crate::vxfw::{
    Center, DrawContext, Event, EventContext, Surface, Text, TextAlign, Widget, WidgetRef,
};

/// The four style states a [`Button`] can render in.
///
/// Each uses an explicit fg/bg pair so terminals that do not render
/// reverse-video on blank cells still show the full button area.
#[derive(Debug, Clone, Copy)]
pub struct ButtonStyle {
    pub default: Style,
    pub mouse_down: Style,
    pub hover: Style,
    pub focus: Style,
}

impl Default for ButtonStyle {
    fn default() -> ButtonStyle {
        ButtonStyle {
            default: Style {
                fg: Color::Index(0),
                bg: Color::Index(7),
                ..Style::default()
            },
            mouse_down: Style {
                fg: Color::Index(15),
                bg: Color::Index(4),
                ..Style::default()
            },
            hover: Style {
                fg: Color::Index(0),
                bg: Color::Index(3),
                ..Style::default()
            },
            focus: Style {
                fg: Color::Index(15),
                bg: Color::Index(5),
                ..Style::default()
            },
        }
    }
}

/// A clickable, focusable button.
///
/// It renders its `label` centered, filling the available space with the
/// state-selected style. A click fires on Enter, Ctrl-J, or a mouse press
/// followed by a release while the pointer is still over the button.
///
/// NOTE: Upstream pairs the click handler with a separate `userdata` pointer.
/// We drop that split: `on_click` is a `Box<dyn FnMut(&mut EventContext)>`, so
/// the closure captures whatever state it needs directly.
pub struct Button {
    pub label: String,
    pub on_click: Box<dyn FnMut(&mut EventContext)>,
    pub style: ButtonStyle,
    /// A mouse press landed on the button and has not yet been released.
    mouse_down: bool,
    /// The pointer is currently over the button.
    has_mouse: bool,
    focused: bool,
}

impl Button {
    /// Builds a button with the default style states.
    pub fn new(
        label: impl Into<String>,
        on_click: impl FnMut(&mut EventContext) + 'static,
    ) -> Button {
        Button {
            label: label.into(),
            on_click: Box::new(on_click),
            style: ButtonStyle::default(),
            mouse_down: false,
            has_mouse: false,
            focused: false,
        }
    }

    /// Invokes the click callback, then consumes the event.
    fn do_click(&mut self, ctx: &mut EventContext) {
        (self.on_click)(ctx);
        ctx.consume_event = true;
    }
}

impl Widget for Button {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let style = if self.mouse_down {
            self.style.mouse_down
        } else if self.has_mouse {
            self.style.hover
        } else if self.focused {
            self.style.focus
        } else {
            self.style.default
        };

        let label: WidgetRef = Rc::new(RefCell::new(Text {
            style,
            text_align: TextAlign::Center,
            ..Text::new(self.label.clone())
        }));
        // Draw Center directly: the Button is the widget that owns this
        // surface, so we reuse Center's size and children but stamp neither
        // Center nor its surface with an identity.
        let mut center = Center { child: label };
        let surf = center.draw(ctx);

        let mut button_surf = Surface::with_children(surf.size, surf.children);
        let base = Cell {
            style,
            ..Cell::default()
        };
        for cell in &mut button_surf.buffer {
            *cell = base.clone();
        }
        button_surf
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::KeyPress(key) => {
                if key.matches(Key::ENTER, Modifiers::empty())
                    || key.matches(u32::from('j'), Modifiers::CTRL)
                {
                    self.do_click(ctx);
                }
            }
            Event::Mouse(mouse) => {
                if self.mouse_down && mouse.kind == mouse::Type::Release {
                    self.mouse_down = false;
                    self.do_click(ctx);
                    return;
                }
                if mouse.kind == mouse::Type::Press && mouse.button == mouse::Button::Left {
                    self.mouse_down = true;
                    ctx.consume_and_redraw();
                    return;
                }
                ctx.consume_event();
            }
            Event::MouseEnter => {
                // Implicit redraw via consume_and_redraw.
                self.has_mouse = true;
                ctx.set_mouse_shape(mouse::Shape::Pointer);
                ctx.consume_and_redraw();
            }
            Event::MouseLeave => {
                self.has_mouse = false;
                self.mouse_down = false;
                ctx.set_mouse_shape(mouse::Shape::Default);
            }
            Event::FocusIn => {
                self.focused = true;
                ctx.redraw = true;
            }
            Event::FocusOut => {
                self.focused = false;
                ctx.redraw = true;
            }
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell as StdCell;

    use super::*;
    use crate::gwidth;
    use crate::mouse::Mouse;
    use crate::vxfw::{MaxSize, Size, draw_widget};

    #[test]
    fn button() {
        // Capture a click counter in the callback.
        let count = Rc::new(StdCell::new(0u32));
        let count_cb = Rc::clone(&count);

        let button: WidgetRef = Rc::new(RefCell::new(Button::new("Test Button", move |ctx| {
            count_cb.set(count_cb.get().saturating_add(1));
            ctx.consume_and_redraw();
        })));

        let mut ctx = EventContext::new();

        // A synthetic left-button mouse press, reused with varying type.
        let mut mouse = Mouse {
            col: 0,
            row: 0,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::Left,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        };
        button
            .borrow_mut()
            .handle_event(&mut ctx, &Event::Mouse(mouse));
        // A press alone does not click.
        assert_eq!(count.get(), 0);

        // A release after a press clicks.
        mouse.kind = mouse::Type::Release;
        button
            .borrow_mut()
            .handle_event(&mut ctx, &Event::Mouse(mouse));
        assert_eq!(count.get(), 1);

        // Another press, then the mouse leaves, then it comes back and we get
        // the release. The press was not registered on us, so no click.
        mouse.kind = mouse::Type::Press;
        button
            .borrow_mut()
            .handle_event(&mut ctx, &Event::Mouse(mouse));
        button
            .borrow_mut()
            .handle_event(&mut ctx, &Event::MouseLeave);
        mouse.kind = mouse::Type::Release;
        button
            .borrow_mut()
            .handle_event(&mut ctx, &Event::Mouse(mouse));
        assert_eq!(count.get(), 1);

        // An Enter keypress clicks.
        button.borrow_mut().handle_event(
            &mut ctx,
            &Event::KeyPress(Key {
                codepoint: Key::ENTER,
                ..Key::default()
            }),
        );
        assert_eq!(count.get(), 2);

        // Draw the button into a 13x3 area.
        let draw_ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(13),
                height: Some(3),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };
        let surface = draw_widget(&button, &draw_ctx);

        // The button fills the available space.
        assert_eq!(surface.size.width, 13);
        assert_eq!(surface.size.height, 3);

        // It has one child, the label, centered.
        assert_eq!(surface.children.len(), 1);
        assert_eq!(surface.children[0].origin.row, 1);
        assert_eq!(surface.children[0].origin.col, 1);
    }
}
