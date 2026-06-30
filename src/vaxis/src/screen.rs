//! The front buffer: a flat row-major cell grid plus cursor and width-method
//! state.
//!
//! [`Screen`] is the buffer the application draws into through [`crate::window`]
//! handles. It owns a contiguous `Vec<Cell>` of `width * height` cells in
//! row-major order. Allocation and teardown collapse into [`Screen::new`] and
//! `Drop`, replacing upstream's explicit allocator threading.

use crate::cell::{Cell, CursorShape};
use crate::gwidth::Method;
use crate::mouse::Shape as MouseShape;

/// A cursor position in cell coordinates, 0-indexed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
}

/// The front cell buffer plus cursor, shape, and width-method state.
#[derive(Debug, Clone)]
pub struct Screen {
    pub width: u16,
    pub height: u16,

    pub width_pix: u16,
    pub height_pix: u16,

    /// Row-major cell grid, `width * height` cells long.
    pub buf: Vec<Cell>,

    pub cursor: Cursor,
    pub cursor_vis: bool,
    pub cursor_secondary: Vec<Cursor>,

    /// Width-measurement method used by [`crate::window::Window::gwidth`] and
    /// the print engine. Defaults to `wcwidth`, matching upstream.
    pub width_method: Method,

    pub mouse_shape: MouseShape,
    pub cursor_shape: CursorShape,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            width_pix: 0,
            height_pix: 0,
            buf: Vec::new(),
            cursor: Cursor::default(),
            cursor_vis: false,
            cursor_secondary: Vec::new(),
            width_method: Method::Wcwidth,
            mouse_shape: MouseShape::default(),
            cursor_shape: CursorShape::default(),
        }
    }
}

impl Screen {
    /// Allocates a screen sized to `winsize` and fills it with default cells.
    pub fn new(winsize: crate::Winsize) -> Self {
        let w = winsize.cols;
        let h = winsize.rows;
        // usize math avoids the u16*u16 overflow upstream guards with @intCast.
        let len = usize::from(w) * usize::from(h);
        Self {
            width: w,
            height: h,
            width_pix: winsize.x_pixel,
            height_pix: winsize.y_pixel,
            buf: vec![Cell::default(); len],
            ..Self::default()
        }
    }

    /// Writes `cell` at the 0-indexed `(col, row)`. Out-of-bounds writes are
    /// silently ignored.
    pub fn write_cell(&mut self, col: u16, row: u16, cell: Cell) {
        if col >= self.width || row >= self.height {
            return;
        }
        let i = usize::from(row) * usize::from(self.width) + usize::from(col);
        debug_assert!(i < self.buf.len());
        self.buf[i] = cell;
    }

    /// Returns a copy of the cell at the 0-indexed `(col, row)`, or `None` when
    /// out of bounds.
    pub fn read_cell(&self, col: u16, row: u16) -> Option<Cell> {
        if col >= self.width || row >= self.height {
            return None;
        }
        let i = usize::from(row) * usize::from(self.width) + usize::from(col);
        debug_assert!(i < self.buf.len());
        Some(self.buf[i].clone())
    }

    /// Resets every cell to the default cell.
    pub fn clear(&mut self) {
        for cell in &mut self.buf {
            *cell = Cell::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Character;

    fn winsize(cols: u16, rows: u16) -> crate::Winsize {
        crate::Winsize {
            rows,
            cols,
            x_pixel: 0,
            y_pixel: 0,
        }
    }

    #[test]
    fn write_read_round_trip() {
        let mut screen = Screen::new(winsize(4, 3));
        let cell = Cell {
            char: Character::new("Z", 1),
            ..Default::default()
        };
        screen.write_cell(2, 1, cell.clone());
        assert_eq!(screen.read_cell(2, 1), Some(cell));
    }

    #[test]
    fn out_of_bounds_read_write_are_ignored() {
        let mut screen = Screen::new(winsize(2, 2));
        let cell = Cell {
            char: Character::new("Q", 1),
            ..Default::default()
        };
        // Out-of-bounds write must not panic and must not change the buffer.
        screen.write_cell(2, 0, cell.clone());
        screen.write_cell(0, 2, cell);
        assert_eq!(screen.read_cell(2, 0), None);
        assert_eq!(screen.read_cell(0, 2), None);
        assert_eq!(screen.read_cell(0, 0), Some(Cell::default()));
    }

    #[test]
    fn clear_resets_all_cells() {
        let mut screen = Screen::new(winsize(2, 2));
        screen.write_cell(
            0,
            0,
            Cell {
                char: Character::new("X", 1),
                ..Default::default()
            },
        );
        screen.clear();
        for row in 0..2 {
            for col in 0..2 {
                assert_eq!(screen.read_cell(col, row), Some(Cell::default()));
            }
        }
    }
}
