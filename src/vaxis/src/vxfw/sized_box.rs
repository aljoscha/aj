//! [`SizedBox`]: draws its child at a target size, clamped to the constraints.

use crate::vxfw::{DrawContext, MaxSize, Size, Surface, Widget, WidgetRef, draw_widget};

/// Tries to draw its child at `size`, shrinking to fit the incoming
/// constraints.
///
/// `SizedBox` is transparent: it returns the child's surface directly rather
/// than wrapping it. An unbounded incoming max on an axis falls back to the
/// target size on that axis, so the child is always drawn against a bounded
/// max.
pub struct SizedBox {
    pub child: WidgetRef,
    pub size: Size,
}

impl Widget for SizedBox {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max_width = ctx.max.width.unwrap_or(self.size.width);
        let max_height = ctx.max.height.unwrap_or(self.size.height);
        let min = Size {
            width: ctx.min.width.max(self.size.width).min(max_width),
            height: ctx.min.height.max(self.size.height).min(max_height),
        };
        let max = MaxSize {
            width: Some(max_width),
            height: Some(max_height),
        };
        draw_widget(&self.child, &ctx.with_constraints(min, max))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::gwidth;

    /// Records the constraints it was drawn with, and reports its size as the
    /// minimum it was given.
    struct TestWidget {
        min: Size,
        max: MaxSize,
    }

    impl Widget for TestWidget {
        fn draw(&mut self, ctx: &DrawContext) -> Surface {
            self.min = ctx.min;
            self.max = ctx.max;
            Surface {
                size: ctx.min,
                widget: None,
                cursor: None,
                buffer: Vec::new(),
                children: Vec::new(),
            }
        }
    }

    #[test]
    fn sized_box() {
        let mut draw_ctx = DrawContext {
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

        let test_widget = Rc::new(RefCell::new(TestWidget {
            min: Size::default(),
            max: MaxSize::default(),
        }));
        // Cloned as the concrete type; the struct field coerces it to `WidgetRef`.
        let child = Rc::clone(&test_widget);
        let sized_box: WidgetRef = Rc::new(RefCell::new(SizedBox {
            child,
            size: Size {
                width: 10,
                height: 10,
            },
        }));

        {
            // Smaller than the constraints, so we get the desired size.
            let result = draw_widget(&sized_box, &draw_ctx);
            assert_eq!(
                result.size,
                Size {
                    width: 10,
                    height: 10
                }
            );
        }

        {
            // A tighter max height shrinks the box on that axis.
            draw_ctx.max.height = Some(8);
            let result = draw_widget(&sized_box, &draw_ctx);
            assert_eq!(
                result.size,
                Size {
                    width: 10,
                    height: 8
                }
            );
        }

        // A tighter max on both axes shrinks the constraints handed to the child.
        draw_ctx.max.width = Some(8);
        let _ = draw_widget(&sized_box, &draw_ctx);
        assert_eq!(
            test_widget.borrow().min,
            Size {
                width: 8,
                height: 8
            }
        );
        assert_eq!(
            test_widget.borrow().max.size(),
            Size {
                width: 8,
                height: 8
            }
        );
    }
}
