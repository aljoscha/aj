//! [`PromptInput`]: a single-line text input drawn behind a leading marker.

use std::cell::RefCell;
use std::rc::Rc;

use crate::cell::{Cell, Character, Style};
use crate::vxfw::{
    DrawContext, EventContext, MaxSize, RelativePoint, Size, SubSurface, Surface, TextField,
    Widget, WidgetRef, draw_widget, to_widget_ref,
};

/// A single-line text input drawn behind a fixed leading marker (e.g. `"> "`).
///
/// The selector overlays share a "type to filter" input. Drawing it behind a
/// marker makes the row read as a prompt and sets the query apart from the
/// results below. The widget owns its [`TextField`]: the field keeps the
/// text, the cursor, and the change callback, while this widget paints the
/// marker and shifts the field right by the marker's display width.
///
/// Hosts focus [`focus_target`](Self::focus_target), not the `PromptInput`
/// itself. The field is drawn as a sub-surface at the marker offset, so its
/// cursor composites through that offset automatically and lands after the
/// marker.
pub struct PromptInput {
    field: Rc<RefCell<TextField>>,
    /// The marker painted at column 0 in [`marker_style`](Self::marker_style).
    pub marker: String,
    /// Style for the marker cells.
    pub marker_style: Style,
}

impl PromptInput {
    /// An empty input behind `marker`, painted in `marker_style`.
    pub fn new(marker: impl Into<String>, marker_style: Style) -> PromptInput {
        PromptInput {
            field: Rc::new(RefCell::new(TextField::new())),
            marker: marker.into(),
            marker_style,
        }
    }

    /// The widget the host should focus while the prompt is active: the inner
    /// field, so its cursor renders and printable keys reach it.
    pub fn focus_target(&self) -> WidgetRef {
        to_widget_ref(Rc::clone(&self.field))
    }

    /// Install the change callback, fired with the field's text after each
    /// edit that actually changes it.
    pub fn set_on_change(&self, on_change: impl FnMut(&mut EventContext, &str) + 'static) {
        self.field.borrow_mut().on_change = Some(Box::new(on_change));
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

        // The field takes the width left of the marker. A marker as wide as
        // the slot leaves nothing, which the field renders as empty.
        let inner_max = MaxSize {
            width: Some(max_width.saturating_sub(marker_width)),
            height: ctx.max.height,
        };
        let field = draw_widget(
            &self.focus_target(),
            &ctx.with_constraints(ctx.min, inner_max),
        );
        let height = field.size.height.max(1);

        let mut surface = Surface::with_size(Size {
            width: max_width,
            height,
        });
        // Paint the marker across the first row. A wide grapheme advances the
        // column by its display width so the field lands flush after it.
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
            surface: field,
            z_index: 0,
        });
        surface
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Color;
    use crate::gwidth;

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
        let mut prompt = PromptInput::new("> ", marker_style);
        prompt.field.borrow_mut().insert_slice_at_cursor("abc");

        let surface = prompt.draw(&draw_ctx(20, 1));
        // The marker sits at columns 0..2 in the marker style.
        assert_eq!(&row_text(&surface)[..2], "> ");
        assert_eq!(surface.buffer[0].char.grapheme(), ">");
        assert_eq!(surface.buffer[0].style, marker_style);

        // The field is a sub-surface shifted right by the marker width.
        assert_eq!(surface.children.len(), 1);
        assert_eq!(surface.children[0].origin, RelativePoint { row: 0, col: 2 });
        assert_eq!(row_text(&surface.children[0].surface).trim_end(), "abc");
    }

    #[test]
    fn cursor_shifts_right_by_the_marker_width() {
        let mut prompt = PromptInput::new("> ", Style::default());
        prompt.field.borrow_mut().insert_slice_at_cursor("hi");

        let surface = prompt.draw(&draw_ctx(20, 1));
        // The field owns the cursor; composited at the marker offset it lands
        // after the two-cell marker plus the two typed graphemes.
        let field = &surface.children[0];
        let cursor = field.surface.cursor.expect("field requests a cursor");
        assert_eq!(u16::try_from(field.origin.col).unwrap() + cursor.col, 4);
    }

    #[test]
    fn field_is_narrowed_by_the_marker_width() {
        let mut prompt = PromptInput::new("> ", Style::default());
        let surface = prompt.draw(&draw_ctx(20, 1));
        // Full width overall, field left with the remainder past the marker.
        assert_eq!(surface.size.width, 20);
        assert_eq!(surface.children[0].surface.size.width, 18);
    }
}
