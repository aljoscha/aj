//! [`Border`]: draws a rounded frame with optional labels around a child.

use crate::cell::{Cell, Character, Style};
use crate::vxfw::{
    DrawContext, MaxSize, RelativePoint, Size, SubSurface, Surface, Widget, WidgetRef, draw_widget,
};

/// Where a [`BorderLabel`] sits on the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorderAlignment {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    BottomCenter,
    BottomRight,
}

/// A label drawn into the top or bottom edge of a [`Border`].
#[derive(Debug, Clone)]
pub struct BorderLabel {
    pub text: String,
    pub alignment: BorderAlignment,
}

/// Draws a rounded border around its child, plus any labels.
///
/// With a bounded max, the child's max is shrunk by the two border cells before
/// drawing. With an unbounded max the child is drawn first and the border wraps
/// its resulting size. The frame and labels are drawn into the border's own
/// buffer.
pub struct Border {
    pub child: WidgetRef,
    pub style: Style,
    pub labels: Vec<BorderLabel>,
}

impl Widget for Border {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max_width = ctx.max.width.map(|w| w.saturating_sub(2));
        let max_height = ctx.max.height.map(|h| h.saturating_sub(2));

        let child_ctx = ctx.with_constraints(
            ctx.min,
            MaxSize {
                width: max_width,
                height: max_height,
            },
        );
        let child = draw_widget(&self.child, &child_ctx);
        let child_size = child.size;

        let children = vec![SubSurface {
            origin: RelativePoint { col: 1, row: 1 },
            z_index: 0,
            surface: child,
        }];

        let size = Size {
            width: child_size.width + 2,
            height: child_size.height + 2,
        };
        let mut surf = Surface::with_children(size, children);

        let right_edge = size.width.saturating_sub(1);
        let bottom_edge = size.height.saturating_sub(1);

        let style = self.style;
        let glyph = |g: &str| Cell {
            char: Character::new(g, 1),
            style,
            ..Cell::default()
        };

        surf.write_cell(0, 0, glyph("╭"));
        surf.write_cell(right_edge, 0, glyph("╮"));
        surf.write_cell(right_edge, bottom_edge, glyph("╯"));
        surf.write_cell(0, bottom_edge, glyph("╰"));

        let mut col: u16 = 1;
        while col < right_edge {
            surf.write_cell(col, 0, glyph("─"));
            surf.write_cell(col, bottom_edge, glyph("─"));
            col += 1;
        }

        let mut row: u16 = 1;
        while row < bottom_edge {
            surf.write_cell(0, row, glyph("│"));
            surf.write_cell(right_edge, row, glyph("│"));
            row += 1;
        }

        for label in &self.labels {
            let text_len =
                u16::try_from(ctx.string_width(&label.text)).expect("gwidth returns a u16");
            if text_len == 0 {
                continue;
            }

            let text_row = match label.alignment {
                BorderAlignment::TopLeft
                | BorderAlignment::TopCenter
                | BorderAlignment::TopRight => 0,
                BorderAlignment::BottomLeft
                | BorderAlignment::BottomCenter
                | BorderAlignment::BottomRight => bottom_edge,
            };

            let mut text_col = match label.alignment {
                BorderAlignment::TopLeft | BorderAlignment::BottomLeft => 1,
                BorderAlignment::TopCenter | BorderAlignment::BottomCenter => {
                    (size.width.saturating_sub(text_len) / 2).max(1)
                }
                BorderAlignment::TopRight | BorderAlignment::BottomRight => {
                    size.width.saturating_sub(1).saturating_sub(text_len).max(1)
                }
            };

            for item in ctx.grapheme_iterator(&label.text) {
                let text = item.bytes(&label.text);
                let width = u16::try_from(ctx.string_width(text)).expect("gwidth returns a u16");
                surf.write_cell(
                    text_col,
                    text_row,
                    Cell {
                        char: Character::new(
                            text,
                            u8::try_from(width).expect("grapheme width fits a u8"),
                        ),
                        style,
                        ..Cell::default()
                    },
                );
                text_col += width;
            }
        }

        surf
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
    fn border() {
        // "abc" lays out as height=1, width=3.
        let text: WidgetRef = Rc::new(RefCell::new(Text::new("abc")));
        let border: WidgetRef = Rc::new(RefCell::new(Border {
            child: Rc::clone(&text),
            style: Style::default(),
            labels: Vec::new(),
        }));

        // Border draws itself tightly around the child.
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

        let surface = draw_widget(&border, &ctx);
        // Border is the child size plus two cells on each axis.
        assert_eq!(surface.size.width, 5);
        assert_eq!(surface.size.height, 3);
        assert_eq!(surface.children.len(), 1);
        let child = &surface.children[0];
        assert_eq!(child.surface.size.width, 3);
        assert_eq!(child.surface.size.height, 1);
    }
}
