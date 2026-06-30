//! [`SplitView`]: two side-by-side panes divided by a draggable vertical
//! separator.

use crate::cell::{Cell, Character, Style};
use crate::mouse;
use crate::vxfw::{
    DrawContext, Event, EventContext, MaxSize, RelativePoint, Size, SubSurface, Surface, Widget,
    WidgetRef, draw_widget,
};

/// Which pane the configured `width`/`min_width`/`max_width` constrain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Constrain {
    Lhs,
    Rhs,
}

/// Two panes split by a draggable vertical separator.
///
/// The constrained side is sized to `width` (clamped to `min_width`/`max_width`
/// while dragging); the other side fills the rest. Dragging the separator sets
/// the `ew-resize` mouse shape and updates `width`.
pub struct SplitView {
    pub lhs: WidgetRef,
    pub rhs: WidgetRef,
    pub constrain: Constrain,
    pub style: Style,
    /// Minimum width for the constrained side.
    pub min_width: u16,
    /// Maximum width for the constrained side.
    pub max_width: Option<u16>,
    /// Target width to draw the constrained side at.
    pub width: u16,
    /// The last max width seen during draw. Needed to map mouse columns to a
    /// separator position when the constrained side is the right pane.
    last_max_width: Option<u16>,
    pressed: bool,
    mouse_set: bool,
}

impl SplitView {
    /// Builds a left-constrained split of `lhs` and `rhs` at the given `width`.
    pub fn new(lhs: WidgetRef, rhs: WidgetRef, width: u16) -> SplitView {
        SplitView {
            lhs,
            rhs,
            constrain: Constrain::Lhs,
            style: Style::default(),
            min_width: 0,
            max_width: None,
            width,
            last_max_width: None,
            pressed: false,
            mouse_set: false,
        }
    }
}

impl Widget for SplitView {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // Fills the entire space.
        let max = ctx.max.size();
        self.width = self.width.min(max.width);
        self.last_max_width = Some(max.width);

        // The constrained side is `width` wide; the other gets the remainder
        // minus the separator column.
        let constrained_min = Size {
            width: self.width,
            height: max.height,
        };
        let constrained_max = MaxSize::from_size(constrained_min);
        let unconstrained_min = Size {
            width: max.width.saturating_sub(self.width).saturating_sub(1),
            height: max.height,
        };
        let unconstrained_max = MaxSize::from_size(unconstrained_min);

        let mut children: Vec<SubSurface> = Vec::with_capacity(2);

        match self.constrain {
            Constrain::Lhs => {
                if constrained_min.width > 0 && constrained_min.height > 0 {
                    let lhs_ctx = ctx.with_constraints(constrained_min, constrained_max);
                    let lhs_surface = draw_widget(&self.lhs, &lhs_ctx);
                    children.push(SubSurface {
                        origin: RelativePoint { row: 0, col: 0 },
                        surface: lhs_surface,
                        z_index: 0,
                    });
                }
                if unconstrained_min.width > 0 && unconstrained_min.height > 0 {
                    let rhs_ctx = ctx.with_constraints(unconstrained_min, unconstrained_max);
                    let rhs_surface = draw_widget(&self.rhs, &rhs_ctx);
                    children.push(SubSurface {
                        origin: RelativePoint {
                            row: 0,
                            col: i32::from(self.width) + 1,
                        },
                        surface: rhs_surface,
                        z_index: 0,
                    });
                }
                let mut surface = Surface::with_children(max, children);
                for row in 0..max.height {
                    surface.write_cell(self.width, row, separator(self.style));
                }
                surface
            }
            Constrain::Rhs => {
                if unconstrained_min.width > 0 && unconstrained_min.height > 0 {
                    let lhs_ctx = ctx.with_constraints(unconstrained_min, unconstrained_max);
                    let lhs_surface = draw_widget(&self.lhs, &lhs_ctx);
                    children.push(SubSurface {
                        origin: RelativePoint { row: 0, col: 0 },
                        surface: lhs_surface,
                        z_index: 0,
                    });
                }
                if constrained_min.width > 0 && constrained_min.height > 0 {
                    let rhs_ctx = ctx.with_constraints(constrained_min, constrained_max);
                    let rhs_surface = draw_widget(&self.rhs, &rhs_ctx);
                    children.push(SubSurface {
                        origin: RelativePoint {
                            row: 0,
                            col: i32::from(unconstrained_min.width) + 1,
                        },
                        surface: rhs_surface,
                        z_index: 0,
                    });
                }
                let mut surface = Surface::with_children(max, children);
                let sep_col = max.width.saturating_sub(self.width).saturating_sub(1);
                for row in 0..max.height {
                    surface.write_cell(sep_col, row, separator(self.style));
                }
                surface
            }
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let mouse = match event {
            Event::MouseLeave => {
                self.pressed = false;
                return;
            }
            Event::Mouse(m) => *m,
            _ => return,
        };

        let separator_col: u16 = match self.constrain {
            Constrain::Lhs => self.width,
            Constrain::Rhs => match self.last_max_width {
                // Needs a prior draw to know where the right-pane separator
                // sits; until then we just request a redraw.
                Some(max) => max.saturating_sub(self.width).saturating_sub(1),
                None => {
                    ctx.redraw = true;
                    return;
                }
            },
        };

        // On the separator, always set the resize shape.
        if i32::from(mouse.col) == i32::from(separator_col) {
            ctx.set_mouse_shape(mouse::Shape::EwResize);
            self.mouse_set = true;
            if mouse.kind == mouse::Type::Press && mouse.button == mouse::Button::Left {
                self.pressed = true;
            }
        } else if self.mouse_set {
            // We set the shape but moved off the separator: restore it.
            ctx.set_mouse_shape(mouse::Shape::Default);
            self.mouse_set = false;
        }

        if mouse.kind == mouse::Type::Release {
            self.pressed = false;
            self.mouse_set = false;
            ctx.set_mouse_shape(mouse::Shape::Default);
        }

        // While pressed, keep the resize shape and update the width.
        if self.pressed {
            ctx.set_mouse_shape(mouse::Shape::EwResize);
            let mouse_col: u16 = if mouse.col < 0 {
                0
            } else {
                u16::try_from(mouse.col).expect("non-negative mouse col fits a u16")
            };
            match self.constrain {
                Constrain::Lhs => {
                    self.width = self.min_width.max(mouse_col);
                    if let Some(max) = self.max_width {
                        self.width = self.width.min(max);
                    }
                }
                Constrain::Rhs => {
                    let last_max = match self.last_max_width {
                        Some(v) => v,
                        None => return,
                    };
                    self.width = last_max
                        .saturating_sub(self.min_width)
                        .min(last_max.saturating_sub(mouse_col).saturating_sub(1));
                    if let Some(max) = self.max_width {
                        self.width = self.width.max(max);
                    }
                }
            }
            ctx.consume_event = true;
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// The vertical separator glyph in `style`.
fn separator(style: Style) -> Cell {
    Cell {
        char: Character::new("│", 1),
        style,
        ..Cell::default()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::gwidth;
    use crate::mouse::Mouse;
    use crate::vxfw::{Command, Text, draw_widget};

    #[test]
    fn split_view() {
        let draw_ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(16),
                height: Some(16),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };

        let lhs: WidgetRef = Rc::new(RefCell::new(Text::new("Left hand side")));
        let rhs: WidgetRef = Rc::new(RefCell::new(Text::new("Right hand side")));

        let split = Rc::new(RefCell::new(SplitView::new(lhs, rhs, 8)));
        let split_concrete = Rc::clone(&split);
        let split_widget: WidgetRef = split_concrete;

        {
            let surface = draw_widget(&split_widget, &draw_ctx);
            // SplitView expands to fill the space.
            assert_eq!(
                surface.size,
                Size {
                    width: 16,
                    height: 16
                }
            );
            // Two children.
            assert_eq!(surface.children.len(), 2);
            // The left child width equals SplitView.width.
            assert_eq!(surface.children[0].surface.size.width, split.borrow().width);
        }

        // A mouse press on the separator (at col == width).
        let mut mouse = Mouse {
            col: i16::try_from(split.borrow().width).expect("width fits an i16"),
            row: 0,
            xoffset: 0,
            yoffset: 0,
            button: mouse::Button::Left,
            mods: mouse::Modifiers::empty(),
            kind: mouse::Type::Press,
        };

        let mut ctx = EventContext::new();
        split_widget
            .borrow_mut()
            .handle_event(&mut ctx, &Event::Mouse(mouse));
        // A command to change the mouse shape, a redraw, and a pressed state.
        assert!(matches!(ctx.cmds[0], Command::SetMouseShape(_)));
        assert!(ctx.redraw);
        assert!(split.borrow().pressed);

        // Moving the mouse updates the width.
        mouse.col = 2;
        mouse.kind = mouse::Type::Drag;
        split_widget
            .borrow_mut()
            .handle_event(&mut ctx, &Event::Mouse(mouse));
        assert!(ctx.redraw);
        assert!(split.borrow().pressed);
        let mouse_col: u16 = if mouse.col < 0 {
            0
        } else {
            u16::try_from(mouse.col).expect("non-negative mouse col fits a u16")
        };
        assert_eq!(mouse_col, split.borrow().width);
    }
}
