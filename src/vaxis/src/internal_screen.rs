//! The back buffer: owns grapheme bytes and serves as the previous-frame
//! snapshot for diff rendering.
//!
//! Unlike the front [`crate::screen::Screen`], whose cells borrow their
//! grapheme bytes (under D1 Option A, inline), the back buffer's
//! [`InternalCell`] owns its grapheme, uri, and uri-id bytes. The renderer
//! compares the freshly drawn front cell against the stored back cell with the
//! asymmetric [`InternalCell::eql`] to decide what to emit.
//!
//! Upstream allocates the owned bytes from a per-screen arena as a performance
//! optimization. Owning plain `String`/`CompactString` per cell is the
//! faithful-behavior equivalent: the bytes live as long as the cell and are
//! overwritten in place on each `write_cell`.

use crate::cell::{Cell, CursorShape, Grapheme, Scale, Style};
use crate::mouse::Shape as MouseShape;

/// A back-buffer cell that owns its grapheme, uri, and uri-id bytes.
///
/// `scale`, `skip`, and `skipped` are render-state bookkeeping carried for
/// parity with the front cell. They are intentionally excluded from
/// [`InternalCell::eql`].
#[derive(Debug, Clone)]
pub struct InternalCell {
    /// Owned grapheme cluster bytes.
    pub char: Grapheme,
    pub style: Style,
    /// Owned OSC 8 hyperlink uri bytes.
    pub uri: String,
    /// Owned OSC 8 hyperlink id bytes (the `params` of a front cell's link).
    pub uri_id: String,
    /// Set when this cell was skipped because a preceding wide character
    /// covered it.
    pub skipped: bool,
    pub default: bool,
    /// Set when this cell should not be rendered this round because a scaled
    /// cell printed over it.
    pub skip: bool,
    pub scale: Scale,
}

impl Default for InternalCell {
    fn default() -> Self {
        // Matches an initialized back-buffer cell: a single space, marked
        // default. The space mirrors upstream's `init`, which seeds every cell
        // with one space byte.
        Self {
            char: Grapheme::const_new(" "),
            style: Style::default(),
            uri: String::new(),
            uri_id: String::new(),
            skipped: false,
            default: true,
            skip: false,
            scale: Scale::default(),
        }
    }
}

impl InternalCell {
    /// Asymmetric equality against a front [`Cell`], as used by the renderer's
    /// diff.
    ///
    /// Fast-paths when both cells are the default cell. Otherwise compares the
    /// grapheme bytes, the [`Style`] (via [`Style::eql`]), and the uri and
    /// uri-id bytes. `scale`, `skip`, `skipped`, and any image are
    /// deliberately ignored: the diff treats those separately.
    pub fn eql(&self, cell: &Cell) -> bool {
        if self.default && cell.default {
            return true;
        }
        self.char.as_str() == cell.char.grapheme()
            && self.style.eql(&cell.style)
            && self.uri == cell.link.uri
            && self.uri_id == cell.link.params
    }
}

/// The back cell buffer plus cursor and shape state.
#[derive(Debug, Clone)]
pub struct InternalScreen {
    pub width: u16,
    pub height: u16,

    /// Row-major cell grid, `width * height` cells long.
    pub buf: Vec<InternalCell>,

    pub cursor_vis: bool,
    pub cursor_shape: CursorShape,
    pub mouse_shape: MouseShape,
}

impl InternalScreen {
    /// Allocates a `w * h` back buffer with every cell set to the default
    /// (a single space, marked default).
    pub fn new(w: u16, h: u16) -> Self {
        let len = usize::from(w) * usize::from(h);
        Self {
            width: w,
            height: h,
            buf: vec![InternalCell::default(); len],
            cursor_vis: false,
            cursor_shape: CursorShape::default(),
            mouse_shape: MouseShape::default(),
        }
    }

    /// Copies `cell`'s grapheme, uri, uri-id, style, and default flag into the
    /// cell at the 0-indexed `(col, row)`. Out-of-bounds writes are ignored.
    pub fn write_cell(&mut self, col: u16, row: u16, cell: &Cell) {
        if self.width <= col {
            return;
        }
        if self.height <= row {
            return;
        }
        let i = usize::from(row) * usize::from(self.width) + usize::from(col);
        debug_assert!(i < self.buf.len());
        let dst = &mut self.buf[i];
        dst.char.clear();
        dst.char.push_str(cell.char.grapheme());
        dst.uri.clear();
        dst.uri.push_str(&cell.link.uri);
        dst.uri_id.clear();
        dst.uri_id.push_str(&cell.link.params);
        dst.style = cell.style;
        dst.default = cell.default;
    }

    /// Returns the cell at the 0-indexed `(col, row)` as a front [`Cell`]
    /// borrowing the stored bytes, or `None` when out of bounds.
    ///
    /// The returned `Character` width is 0: the back buffer does not track
    /// width, and callers that need it measure the grapheme themselves.
    pub fn read_cell(&self, col: u16, row: u16) -> Option<Cell> {
        if self.width <= col {
            return None;
        }
        if self.height <= row {
            return None;
        }
        let i = usize::from(row) * usize::from(self.width) + usize::from(col);
        debug_assert!(i < self.buf.len());
        let cell = &self.buf[i];
        Some(Cell {
            char: crate::cell::Character::new(cell.char.as_str(), 0),
            style: cell.style,
            link: crate::cell::Hyperlink {
                uri: cell.uri.clone(),
                params: cell.uri_id.clone(),
            },
            default: cell.default,
            ..Cell::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Character;

    #[test]
    fn out_of_bounds_read_write_are_ignored() {
        let mut screen = InternalScreen::new(2, 2);

        let sentinel = Cell {
            char: Character::new("A", 1),
            ..Cell::default()
        };
        screen.write_cell(0, 1, &sentinel);

        // Out-of-bounds write is dropped, not stored anywhere.
        let oob_cell = Cell {
            char: Character::new("X", 1),
            ..Cell::default()
        };
        screen.write_cell(2, 0, &oob_cell);

        let read_back = screen.read_cell(0, 1).expect("in-bounds read");
        assert_eq!(read_back.char.grapheme(), "A");
        assert!(screen.read_cell(2, 0).is_none());
    }
}
