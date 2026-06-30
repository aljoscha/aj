//! [`Padding`]: insets a single child by per-side padding.

use crate::vxfw::{
    DrawContext, MaxSize, RelativePoint, Size, SubSurface, Surface, Widget, WidgetRef, draw_widget,
};

/// Per-side padding in cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PadValues {
    pub left: u16,
    pub right: u16,
    pub top: u16,
    pub bottom: u16,
}

impl PadValues {
    /// Equal padding on all sides, with the vertical padding halved.
    ///
    /// The halving is deliberate: terminal cells are roughly twice as tall as
    /// they are wide, so half the vertical count approximates visually equal
    /// padding.
    pub fn all(padding: u16) -> PadValues {
        PadValues {
            left: padding,
            right: padding,
            top: padding / 2,
            bottom: padding / 2,
        }
    }

    /// Padding on the left and right only.
    pub fn horizontal(padding: u16) -> PadValues {
        PadValues {
            left: padding,
            right: padding,
            top: 0,
            bottom: 0,
        }
    }

    /// Padding on the top and bottom only.
    pub fn vertical(padding: u16) -> PadValues {
        PadValues {
            left: 0,
            right: 0,
            top: padding,
            bottom: padding,
        }
    }
}

/// Insets its child by [`PadValues`], shrinking the child's constraints by the
/// padding and growing its own size by the same.
pub struct Padding {
    pub child: WidgetRef,
    pub padding: PadValues,
}

impl Widget for Padding {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let pad = self.padding;
        // Horizontal/vertical padding can only be applied against a bounded max
        // on that axis.
        if pad.left > 0 || pad.right > 0 {
            debug_assert!(
                ctx.max.width.is_some(),
                "horizontal padding requires a bounded max width"
            );
        }
        if pad.top > 0 || pad.bottom > 0 {
            debug_assert!(
                ctx.max.height.is_some(),
                "vertical padding requires a bounded max height"
            );
        }

        let inner_min = Size {
            width: ctx.min.width.saturating_sub(pad.right + pad.left),
            height: ctx.min.height.saturating_sub(pad.top + pad.bottom),
        };
        let inner_max = MaxSize {
            width: ctx
                .max
                .width
                .map(|max| max.saturating_sub(pad.right + pad.left)),
            height: ctx
                .max
                .height
                .map(|max| max.saturating_sub(pad.top + pad.bottom)),
        };

        let child = draw_widget(&self.child, &ctx.with_constraints(inner_min, inner_max));
        let child_size = child.size;

        let children = vec![SubSurface {
            surface: child,
            z_index: 0,
            origin: RelativePoint {
                row: i32::from(pad.top),
                col: i32::from(pad.left),
            },
        }];

        let size = Size {
            width: child_size.width + (pad.right + pad.left),
            height: child_size.height + (pad.top + pad.bottom),
        };

        Surface {
            size,
            widget: None,
            cursor: None,
            buffer: Vec::new(),
            children,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::gwidth;
    use crate::vxfw::Text;

    #[test]
    fn padding() {
        // "abc" lays out as height=1, width=3.
        let text: WidgetRef = Rc::new(RefCell::new(Text::new("abc")));
        let padding: WidgetRef = Rc::new(RefCell::new(Padding {
            child: Rc::clone(&text),
            padding: PadValues::horizontal(1),
        }));

        let ctx = DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(10),
                height: Some(10),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        };

        let surface = draw_widget(&padding, &ctx);
        // Padding positions a child but draws no cells of its own.
        assert_eq!(surface.buffer.len(), 0);
        assert_eq!(surface.children.len(), 1);
        let child = &surface.children[0];
        // The size is the child plus the horizontal padding.
        assert_eq!(child.surface.size.width + 2, surface.size.width);
        assert_eq!(child.origin.row, 0);
        assert_eq!(child.origin.col, 1);
    }
}
