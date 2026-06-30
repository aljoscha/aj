//! [`Center`]: centers a single child within the parent's bounded max size.

use crate::vxfw::{
    DrawContext, RelativePoint, Size, SubSurface, Surface, Widget, WidgetRef, draw_widget,
};

/// Centers its child both horizontally and vertically.
///
/// Expands to the parent's max size, so the max must be bounded on both axes
/// (see [`Surface`]). The child is drawn with a zero minimum and the parent's
/// max, then placed with a bias toward the top-left when the leftover space is
/// odd.
pub struct Center {
    pub child: WidgetRef,
}

impl Widget for Center {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let child_ctx = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            ctx.max,
        );
        let max_size = ctx.max.size();
        let child = draw_widget(&self.child, &child_ctx);

        let x = max_size.width.saturating_sub(child.size.width) / 2;
        let y = max_size.height.saturating_sub(child.size.height) / 2;

        let children = vec![SubSurface {
            origin: RelativePoint {
                col: i32::from(x),
                row: i32::from(y),
            },
            z_index: 0,
            surface: child,
        }];

        Surface {
            size: max_size,
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
    use crate::vxfw::{MaxSize, Text};

    fn ctx(max: MaxSize) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max,
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        }
    }

    #[test]
    fn center() {
        // "abc" lays out as height=1, width=3.
        let text: WidgetRef = Rc::new(RefCell::new(Text::new("abc")));
        let center: WidgetRef = Rc::new(RefCell::new(Center {
            child: Rc::clone(&text),
        }));

        {
            let surface = draw_widget(
                &center,
                &ctx(MaxSize {
                    width: Some(10),
                    height: Some(10),
                }),
            );
            // Center positions a child but draws no cells of its own.
            assert_eq!(surface.buffer.len(), 0);
            assert_eq!(surface.children.len(), 1);
            // Center fills the max size.
            assert_eq!(
                surface.size,
                Size {
                    width: 10,
                    height: 10
                }
            );
            let child = &surface.children[0];
            assert_eq!(child.surface.size.width, 3);
            assert_eq!(child.surface.size.height, 1);
            // A 1x3 child centered in 10x10 sits at (row 4, col 3), biased
            // toward the top-left.
            assert_eq!(child.origin.row, 4);
            assert_eq!(child.origin.col, 3);
        }

        {
            let surface = draw_widget(
                &center,
                &ctx(MaxSize {
                    width: Some(5),
                    height: Some(3),
                }),
            );
            assert_eq!(surface.buffer.len(), 0);
            assert_eq!(surface.children.len(), 1);
            assert_eq!(
                surface.size,
                Size {
                    width: 5,
                    height: 3
                }
            );
            let child = &surface.children[0];
            assert_eq!(child.surface.size.width, 3);
            assert_eq!(child.surface.size.height, 1);
            // A 1x3 child centered in 3x5 is perfectly centered at (1, 1).
            assert_eq!(child.origin.row, 1);
            assert_eq!(child.origin.col, 1);
        }
    }
}
