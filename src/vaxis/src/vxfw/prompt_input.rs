//! [`PromptInput`]: a single-line input drawn behind a leading marker.

use crate::cell::{Cell, Character, Style};
use crate::vxfw::{
    DrawContext, MaxSize, RelativePoint, Size, SubSurface, Surface, Widget, WidgetRef, draw_widget,
};

/// A single-line input drawn behind a fixed leading marker (e.g. `"> "`).
///
/// The selector overlays share a "type to filter" input. Drawing it behind a
/// marker makes the row read as a prompt and sets the query apart from the
/// results below. This is a thin decoration around a child input, usually a
/// [`TextField`](crate::vxfw::TextField): the child stays the focus target and
/// owns the text, the cursor, and the change callback, while this widget paints
/// the marker and shifts the child right by the marker's display width.
///
/// The child is drawn as a sub-surface at the marker offset, so its cursor
/// composites through that offset automatically and lands after the marker.
/// The marker occupies the first row only, so the child is expected to be a
/// single-line input.
pub struct PromptInput {
    /// The wrapped input. Stays the focus target so its cursor renders (offset
    /// right by the marker) and printable keys reach it.
    pub child: WidgetRef,
    /// The marker painted at column 0 in [`marker_style`](Self::marker_style).
    pub marker: String,
    /// Style for the marker cells.
    pub marker_style: Style,
}

impl PromptInput {
    /// Wrap `child` behind `marker`, painted in `marker_style`.
    pub fn new(child: WidgetRef, marker: impl Into<String>, marker_style: Style) -> PromptInput {
        PromptInput {
            child,
            marker: marker.into(),
            marker_style,
        }
    }
}

impl Widget for PromptInput {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max_width = ctx
            .max
            .width
            .expect("PromptInput requires a bounded max width");
        let marker_width =
            u16::try_from(ctx.string_width(&self.marker)).expect("marker width fits u16");

        // The child takes the width left of the marker. A marker as wide as the
        // slot leaves nothing, which the child renders as empty.
        let inner_max = MaxSize {
            width: Some(max_width.saturating_sub(marker_width)),
            height: ctx.max.height,
        };
        let child = draw_widget(&self.child, &ctx.with_constraints(ctx.min, inner_max));
        let height = child.size.height.max(1);

        let mut surface = Surface::with_size(Size {
            width: max_width,
            height,
        });
        // Paint the marker across the first row. A wide grapheme advances the
        // column by its display width so the child lands flush after it.
        let mut col = 0u16;
        for grapheme in ctx.grapheme_iterator(&self.marker) {
            let g = grapheme.bytes(&self.marker);
            let w = u8::try_from(ctx.string_width(g)).expect("grapheme width fits u8");
            surface.write_cell(
                col,
                0,
                Cell {
                    char: Character::new(g, w),
                    style: self.marker_style,
                    ..Cell::default()
                },
            );
            col += u16::from(w);
        }

        surface.children.push(SubSurface {
            origin: RelativePoint {
                row: 0,
                col: i32::from(marker_width),
            },
            surface: child,
            z_index: 0,
        });
        surface
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::cell::Color;
    use crate::gwidth;
    use crate::vxfw::{TextField, to_widget_ref};

    fn draw_ctx(width: u16, height: u16) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(width),
                height: Some(height),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        }
    }

    /// The row's graphemes concatenated, for locating a column by its text.
    fn row_text(surface: &Surface) -> String {
        surface.buffer.iter().map(|c| c.char.grapheme()).collect()
    }

    #[test]
    fn prompt_input() {
        let marker_style = Style {
            fg: Color::Index(4),
            ..Style::default()
        };
        let field = Rc::new(RefCell::new(TextField::new()));
        field.borrow_mut().insert_slice_at_cursor("abc");
        let mut prompt = PromptInput::new(to_widget_ref(Rc::clone(&field)), "> ", marker_style);

        let surface = prompt.draw(&draw_ctx(20, 1));
        // The marker sits at columns 0..2 in the marker style.
        assert_eq!(&row_text(&surface)[..2], "> ");
        assert_eq!(surface.buffer[0].char.grapheme(), ">");
        assert_eq!(surface.buffer[0].style, marker_style);

        // The child is a sub-surface shifted right by the marker width.
        assert_eq!(surface.children.len(), 1);
        assert_eq!(surface.children[0].origin, RelativePoint { row: 0, col: 2 });
        assert_eq!(row_text(&surface.children[0].surface).trim_end(), "abc");
    }

    #[test]
    fn cursor_shifts_right_by_the_marker_width() {
        let field = Rc::new(RefCell::new(TextField::new()));
        field.borrow_mut().insert_slice_at_cursor("hi");
        let mut prompt = PromptInput::new(to_widget_ref(Rc::clone(&field)), "> ", Style::default());

        let surface = prompt.draw(&draw_ctx(20, 1));
        // The child owns the cursor; composited at the marker offset it lands
        // after the two-cell marker plus the two typed graphemes.
        let child = &surface.children[0];
        let cursor = child.surface.cursor.expect("field requests a cursor");
        assert_eq!(u16::try_from(child.origin.col).unwrap() + cursor.col, 4);
    }

    #[test]
    fn child_is_narrowed_by_the_marker_width() {
        let field = Rc::new(RefCell::new(TextField::new()));
        let mut prompt = PromptInput::new(to_widget_ref(Rc::clone(&field)), "> ", Style::default());
        let surface = prompt.draw(&draw_ctx(20, 1));
        // Full width overall, child left with the remainder past the marker.
        assert_eq!(surface.size.width, 20);
        assert_eq!(surface.children[0].surface.size.width, 18);
    }
}
