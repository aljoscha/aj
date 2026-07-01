//! The terminal emulator's own screen grid.
//!
//! Distinct from the crate's top-level [`crate::screen`]: this grid is the
//! model the VT parser drives. Each [`Cell`] owns its grapheme, hyperlink URI,
//! and link id bytes, and carries a `dirty` flag so [`Screen::copy_to`] can
//! propagate only the cells that changed since the last snapshot.

use crate::cell::{Character, Color, CursorShape, Style, Underline};
use crate::key::KittyFlags;
use crate::widgets::terminal::ansi::Csi;

/// One grid cell.
///
/// `char`, `uri`, and `uri_id` are owned strings that grow and shrink in place.
/// [`Cell::erase`] clears them retaining capacity, so once a cell has held
/// content its buffers are never fully freed. This matches upstream's per-cell
/// `ArrayList`s and keeps steady-state operation allocation-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cell {
    pub char: String,
    pub style: Style,
    pub uri: String,
    pub uri_id: String,
    pub width: u8,
    pub wrapped: bool,
    pub dirty: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            char: String::new(),
            style: Style::default(),
            uri: String::new(),
            uri_id: String::new(),
            width: 1,
            wrapped: false,
            dirty: true,
        }
    }
}

impl Cell {
    /// Resets the cell to a blank space with the given background, retaining
    /// the string buffers' capacity.
    pub fn erase(&mut self, bg: Color) {
        self.char.clear();
        self.char.push(' ');
        self.style = Style {
            bg,
            ..Style::default()
        };
        self.uri.clear();
        self.uri_id.clear();
        self.width = 1;
        self.wrapped = false;
        self.dirty = true;
    }

    /// Copies `src`'s content into this cell, retaining buffer capacity and
    /// marking the cell dirty.
    pub fn copy_from(&mut self, src: &Cell) {
        self.char.clear();
        self.char.push_str(&src.char);
        self.style = src.style;
        self.uri.clear();
        self.uri.push_str(&src.uri);
        self.uri_id.clear();
        self.uri_id.push_str(&src.uri_id);
        self.width = src.width;
        self.wrapped = src.wrapped;
        self.dirty = true;
    }
}

/// The emulator's cursor position and pending state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub style: Style,
    pub uri: String,
    pub uri_id: String,
    pub col: u16,
    pub row: u16,
    /// Set after printing into the last column with wrap enabled: the next
    /// print wraps to the next line before writing.
    pub pending_wrap: bool,
    pub shape: CursorShape,
    pub visible: bool,
}

impl Default for Cursor {
    fn default() -> Self {
        Cursor {
            style: Style::default(),
            uri: String::new(),
            uri_id: String::new(),
            col: 0,
            row: 0,
            pending_wrap: false,
            shape: CursorShape::Default,
            visible: true,
        }
    }
}

impl Cursor {
    /// True when the cursor lies outside the scrolling region on any edge.
    pub fn is_outside_scrolling_region(&self, sr: &ScrollingRegion) -> bool {
        self.row < sr.top || self.row > sr.bottom || self.col < sr.left || self.col > sr.right
    }

    /// True when the cursor lies inside the scrolling region.
    pub fn is_inside_scrolling_region(&self, sr: &ScrollingRegion) -> bool {
        !self.is_outside_scrolling_region(sr)
    }
}

/// The inclusive rectangular region scroll and edit operations act within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollingRegion {
    pub top: u16,
    pub bottom: u16,
    pub left: u16,
    pub right: u16,
}

impl ScrollingRegion {
    /// True when `(col, row)` lies within the region (inclusive on all edges).
    pub fn contains(&self, col: u16, row: u16) -> bool {
        col >= self.left && col <= self.right && row >= self.top && row <= self.bottom
    }
}

/// A row-major grid of [`Cell`]s plus the cursor and scrolling region.
#[derive(Debug, Clone)]
pub struct Screen {
    pub width: u16,
    pub height: u16,
    pub scrolling_region: ScrollingRegion,
    pub buf: Vec<Cell>,
    pub cursor: Cursor,
    pub csi_u_flags: KittyFlags,
}

impl Screen {
    /// Creates a `w` by `h` screen with every cell seeded as a blank space and
    /// the scrolling region covering the whole grid.
    pub fn new(w: u16, h: u16) -> Screen {
        let count = usize::from(w) * usize::from(h);
        let mut buf = Vec::with_capacity(count);
        for _ in 0..count {
            buf.push(Cell {
                char: " ".to_string(),
                ..Cell::default()
            });
        }
        Screen {
            width: w,
            height: h,
            scrolling_region: ScrollingRegion {
                top: 0,
                bottom: h.saturating_sub(1),
                left: 0,
                right: w.saturating_sub(1),
            },
            buf,
            cursor: Cursor::default(),
            csi_u_flags: KittyFlags::empty(),
        }
    }

    /// The flat buffer index of `(row, col)`.
    fn idx(&self, row: u16, col: u16) -> usize {
        usize::from(row) * usize::from(self.width) + usize::from(col)
    }

    /// Copies one cell to another, retaining the destination's buffer capacity.
    fn copy_cell(&mut self, dst: usize, src: usize) {
        if dst == src {
            // NOTE: upstream `copyFrom(self, self)` clears the cell's buffers
            // and then appends from the now-empty source, blanking the char.
            // We reproduce that (it is only reachable from the degenerate
            // `insert_line`-at-bottom path).
            let cell = &mut self.buf[dst];
            cell.char.clear();
            cell.uri.clear();
            cell.uri_id.clear();
            cell.dirty = true;
            return;
        }
        if dst < src {
            let (a, b) = self.buf.split_at_mut(src);
            a[dst].copy_from(&b[0]);
        } else {
            let (a, b) = self.buf.split_at_mut(dst);
            b[0].copy_from(&a[src]);
        }
    }

    /// Copies the cursor and every dirty cell into `dst`, clearing this
    /// screen's dirty flags. Only the grapheme, width, and style are copied.
    ///
    /// NOTE: the primary back screen is taller than `dst` (the front screen) by
    /// the scrollback rows, so we clear every source dirty flag but only write
    /// cells that fall within `dst`. Copying the whole source into a shorter
    /// destination would index out of bounds.
    pub fn copy_to(&mut self, dst: &mut Screen) {
        dst.cursor = self.cursor.clone();
        for i in 0..self.buf.len() {
            if !self.buf[i].dirty {
                continue;
            }
            self.buf[i].dirty = false;
            if i >= dst.buf.len() {
                continue;
            }
            dst.buf[i].char.clear();
            dst.buf[i].char.push_str(&self.buf[i].char);
            dst.buf[i].width = self.buf[i].width;
            dst.buf[i].style = self.buf[i].style;
        }
    }

    /// Reads a cell as a top-level [`crate::cell::Cell`], carrying only its
    /// grapheme, width, and style.
    ///
    /// NOTE: the bounds checks use `<` (not `<=`), an upstream off-by-one that
    /// admits `col == width` / `row == height`. Reproduced; the flat index
    /// still guards the buffer.
    pub fn read_cell(&self, col: usize, row: usize) -> Option<crate::cell::Cell> {
        if usize::from(self.width) < col {
            return None;
        }
        if usize::from(self.height) < row {
            return None;
        }
        let i = row * usize::from(self.width) + col;
        debug_assert!(i < self.buf.len());
        let cell = &self.buf[i];
        Some(crate::cell::Cell {
            char: Character::new(cell.char.as_str(), cell.width),
            style: cell.style,
            ..crate::cell::Cell::default()
        })
    }

    /// True when the cursor is within the scrolling region.
    pub fn within_scrolling_region(&self) -> bool {
        self.scrolling_region
            .contains(self.cursor.col, self.cursor.row)
    }

    /// Writes a grapheme of the given display width at the cursor, advancing
    /// the cursor. `wrap` arms the pending-wrap on the last column.
    pub fn print(&mut self, grapheme: &str, width: u8, wrap: bool) {
        if self.cursor.pending_wrap {
            self.index();
            self.cursor.col = self.scrolling_region.left;
        }
        if self.cursor.col >= self.width {
            return;
        }
        if self.cursor.row >= self.height {
            return;
        }
        let i = self.idx(self.cursor.row, self.cursor.col);
        let cell = &mut self.buf[i];
        cell.char.clear();
        cell.char.push_str(grapheme);
        cell.uri.clear();
        cell.uri.push_str(&self.cursor.uri);
        cell.uri_id.clear();
        cell.uri_id.push_str(&self.cursor.uri_id);
        cell.style = self.cursor.style;
        cell.width = width;
        cell.dirty = true;

        // The wrap check reads the pre-advance column.
        if wrap && self.cursor.col >= self.width - 1 {
            self.cursor.pending_wrap = true;
        }
        self.cursor.col += u16::from(width);
    }

    /// IND: moves down one line, scrolling the region up when at its bottom.
    pub fn index(&mut self) {
        self.cursor.pending_wrap = false;

        if self
            .cursor
            .is_outside_scrolling_region(&self.scrolling_region)
        {
            self.cursor.row = (self.height - 1).min(self.cursor.row + 1);
            return;
        }
        if self.cursor.row == self.scrolling_region.bottom {
            // TODO(aljoscha): scrollback when the region is the whole screen.
            self.delete_line(1);
            return;
        }
        self.cursor.row += 1;
    }

    /// Applies an SGR (Select Graphic Rendition) sequence to the cursor style.
    pub fn sgr(&mut self, seq: &Csi) {
        if seq.params.is_empty() {
            self.cursor.style = Style::default();
            return;
        }

        let mut iter = seq.iterator::<u8>();
        while let Some(ps) = iter.next() {
            match ps {
                0 => self.cursor.style = Style::default(),
                1 => self.cursor.style.bold = true,
                2 => self.cursor.style.dim = true,
                3 => self.cursor.style.italic = true,
                4 => {
                    let kind = if iter.next_is_sub {
                        underline_from_u8(iter.next().unwrap_or(1))
                    } else {
                        Underline::Single
                    };
                    self.cursor.style.ul_style = kind;
                }
                5 => self.cursor.style.blink = true,
                7 => self.cursor.style.reverse = true,
                8 => self.cursor.style.invisible = true,
                9 => self.cursor.style.strikethrough = true,
                21 => self.cursor.style.ul_style = Underline::Double,
                22 => {
                    self.cursor.style.bold = false;
                    self.cursor.style.dim = false;
                }
                23 => self.cursor.style.italic = false,
                24 => self.cursor.style.ul_style = Underline::Off,
                25 => self.cursor.style.blink = false,
                27 => self.cursor.style.reverse = false,
                28 => self.cursor.style.invisible = false,
                29 => self.cursor.style.strikethrough = false,
                30..=37 => self.cursor.style.fg = Color::Index(ps - 30),
                38 => {
                    let Some(color) = read_extended_color(&mut iter) else {
                        return;
                    };
                    self.cursor.style.fg = color;
                }
                39 => self.cursor.style.fg = Color::Default,
                40..=47 => self.cursor.style.bg = Color::Index(ps - 40),
                48 => {
                    let Some(color) = read_extended_color(&mut iter) else {
                        return;
                    };
                    self.cursor.style.bg = color;
                }
                49 => self.cursor.style.bg = Color::Default,
                90..=97 => self.cursor.style.fg = Color::Index(ps - 90 + 8),
                100..=107 => self.cursor.style.bg = Color::Index(ps - 100 + 8),
                _ => continue,
            }
        }
    }

    /// Moves the cursor up `n` rows, clamped to the region top when inside it.
    pub fn cursor_up(&mut self, n: u16) {
        self.cursor.pending_wrap = false;
        if self.within_scrolling_region() {
            self.cursor.row = self
                .cursor
                .row
                .saturating_sub(n)
                .max(self.scrolling_region.top);
        } else {
            self.cursor.row = self.cursor.row.saturating_sub(n);
        }
    }

    /// Moves the cursor left `n` columns, clamped to the region left when
    /// inside it.
    pub fn cursor_left(&mut self, n: u16) {
        self.cursor.pending_wrap = false;
        if self.within_scrolling_region() {
            self.cursor.col = self
                .cursor
                .col
                .saturating_sub(n)
                .max(self.scrolling_region.left);
        } else {
            self.cursor.col = self.cursor.col.saturating_sub(n);
        }
    }

    /// Moves the cursor right `n` columns, clamped to the region right (inside)
    /// or the last column (outside).
    ///
    /// NOTE: upstream adds without saturating and clamps afterward. We
    /// `saturating_add` to avoid an overflow panic on a huge parameter. The
    /// result is identical after the clamp.
    pub fn cursor_right(&mut self, n: u16) {
        self.cursor.pending_wrap = false;
        if self.within_scrolling_region() {
            self.cursor.col = self
                .cursor
                .col
                .saturating_add(n)
                .min(self.scrolling_region.right);
        } else {
            self.cursor.col = self.cursor.col.saturating_add(n).min(self.width - 1);
        }
    }

    /// Moves the cursor down `n` rows, clamped to the region bottom (inside) or
    /// the last row (outside).
    pub fn cursor_down(&mut self, n: usize) {
        self.cursor.pending_wrap = false;
        let sum = usize::from(self.cursor.row) + n;
        let bound = if self.within_scrolling_region() {
            usize::from(self.scrolling_region.bottom)
        } else {
            usize::from(self.height.saturating_sub(1))
        };
        let clamped = bound.min(sum);
        self.cursor.row = u16::try_from(clamped).expect("clamped to a u16 bound");
    }

    /// Erases from the cursor to the end of its row.
    pub fn erase_right(&mut self) {
        self.cursor.pending_wrap = false;
        let bg = self.cursor.style.bg;
        let start = self.idx(self.cursor.row, self.cursor.col);
        let end = self.idx(self.cursor.row, self.width);
        for i in start..end {
            self.buf[i].erase(bg);
        }
    }

    /// Erases from the start of the cursor's row up to and including the
    /// cursor.
    pub fn erase_left(&mut self) {
        self.cursor.pending_wrap = false;
        let bg = self.cursor.style.bg;
        let start = self.idx(self.cursor.row, 0);
        let end = start + usize::from(self.cursor.col) + 1;
        for i in start..end {
            self.buf[i].erase(bg);
        }
    }

    /// Erases the cursor's entire row.
    pub fn erase_line(&mut self) {
        self.cursor.pending_wrap = false;
        let bg = self.cursor.style.bg;
        let start = self.idx(self.cursor.row, 0);
        let end = start + usize::from(self.width);
        for i in start..end {
            self.buf[i].erase(bg);
        }
    }

    /// Erases from the cursor to the end of the screen.
    pub fn erase_below(&mut self) {
        self.erase_right();
        let bg = self.cursor.style.bg;
        let start = self.idx(self.cursor.row, self.width);
        for i in start..self.buf.len() {
            self.buf[i].erase(bg);
        }
    }

    /// Erases from the start of the screen up to and including the cursor.
    pub fn erase_above(&mut self) {
        self.erase_left();
        let bg = self.cursor.style.bg;
        let end = self.idx(self.cursor.row, 0);
        for i in 0..end {
            self.buf[i].erase(bg);
        }
    }

    /// Erases the whole screen.
    pub fn erase_all(&mut self) {
        let bg = self.cursor.style.bg;
        for i in 0..self.buf.len() {
            self.buf[i].erase(bg);
        }
    }

    /// Deletes `n` lines, scrolling the region below the cursor up and blanking
    /// the lines it opens at the bottom.
    pub fn delete_line(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        if !self.within_scrolling_region() {
            return;
        }
        self.cursor.pending_wrap = false;

        let region = self.scrolling_region;
        let width = usize::from(self.width);
        let cnt = usize::from(region.bottom - self.cursor.row + 1).min(n);
        let stride = width * cnt;
        let bg = self.cursor.style.bg;

        let top = usize::from(region.top);
        let bottom = usize::from(region.bottom);
        let left = usize::from(region.left);
        let right = usize::from(region.right);

        for row in top..=bottom {
            for col in left..=right {
                let i = row * width + col;
                if row + cnt > bottom {
                    self.buf[i].erase(bg);
                } else {
                    self.copy_cell(i, i + stride);
                }
            }
        }
    }

    /// Inserts `n` blank lines at the cursor, scrolling the region below it
    /// down.
    pub fn insert_line(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.cursor.pending_wrap = false;
        if !self.within_scrolling_region() {
            return;
        }

        let region = self.scrolling_region;
        let width = usize::from(self.width);
        let adjusted_n = usize::from(region.bottom - self.cursor.row).min(n);
        let stride = width * adjusted_n;
        let bg = self.cursor.style.bg;

        let top = usize::from(region.top);
        let bottom = usize::from(region.bottom);
        let left = usize::from(region.left);
        let right = usize::from(region.right);

        // Shift rows down, from the bottom of the region upward.
        //
        // NOTE: when the cursor is on the region's bottom row `adjusted_n` is 0,
        // so the shift is a no-op (stride 0) that self-copies each cell (which
        // blanks its char). The bottom-up walk with a saturating decrement spins
        // forever when `top` is also 0: the bound `row >= top + adjusted_n`
        // becomes `row >= 0`, always true for usize, and the decrement floors at
        // 0. Because this processes UNTRUSTED child output, an abnormal cursor
        // position must not hang the reader thread, so we break once row 0 has
        // been handled. The normal path (`scroll_down` positions the cursor at
        // the top, `adjusted_n >= 1`) exits before ever reaching row 0 and is
        // unaffected.
        let mut row = bottom;
        while row >= top + adjusted_n {
            for col in left..=right {
                let i = row * width + col;
                self.copy_cell(i, i - stride);
            }
            if row == 0 {
                break;
            }
            row -= 1;
        }

        // Blank the rows opened at the top.
        for row in top..top + adjusted_n {
            for col in left..=right {
                let i = row * width + col;
                self.buf[i].erase(bg);
            }
        }
    }

    /// Deletes `n` characters at the cursor, shifting the rest of the line left.
    ///
    /// NOTE: upstream indexes `buf[col]` / `buf[col + n]` with no row offset, so
    /// this only ever rewrites cells in the first row. Reproduced faithfully.
    pub fn delete_characters(&mut self, n: usize) {
        if !self.within_scrolling_region() {
            return;
        }
        self.cursor.pending_wrap = false;
        let bg = self.cursor.style.bg;
        let right = usize::from(self.scrolling_region.right);
        let mut col = usize::from(self.cursor.col);
        while col <= right {
            if col + n <= right {
                self.copy_cell(col, col + n);
            } else {
                self.buf[col].erase(bg);
            }
            col += 1;
        }
    }

    /// RI: moves up one line, scrolling the region down when at its top.
    pub fn reverse_index(&mut self) {
        let sr = self.scrolling_region;
        if self.cursor.row != sr.top || self.cursor.col < sr.left || self.cursor.col > sr.right {
            self.cursor_up(1);
        } else {
            self.scroll_down(1);
        }
    }

    /// Scrolls the region down `n` lines, preserving the cursor position.
    pub fn scroll_down(&mut self, n: usize) {
        let cur_row = self.cursor.row;
        let cur_col = self.cursor.col;
        let wrap = self.cursor.pending_wrap;

        self.cursor.col = self.scrolling_region.left;
        self.cursor.row = self.scrolling_region.top;
        self.insert_line(n);

        self.cursor.row = cur_row;
        self.cursor.col = cur_col;
        self.cursor.pending_wrap = wrap;
    }
}

/// Maps an SGR underline sub-parameter to an underline style. Unknown values
/// fall back to a single underline.
fn underline_from_u8(n: u8) -> Underline {
    match n {
        0 => Underline::Off,
        1 => Underline::Single,
        2 => Underline::Double,
        3 => Underline::Curly,
        4 => Underline::Dotted,
        5 => Underline::Dashed,
        _ => Underline::Single,
    }
}

/// Reads an extended color parameter for SGR 38/48: either `2;r;g;b` (RGB) or
/// `5;i` (256-color index). Returns `None` on a malformed sequence, which bails
/// out of the whole SGR sequence upstream.
fn read_extended_color(
    iter: &mut crate::widgets::terminal::ansi::ParamIterator<'_, u8>,
) -> Option<Color> {
    let kind = iter.next()?;
    match kind {
        2 => {
            // The first RGB component may be an empty sub-parameter (`38:2::r`),
            // in which case we skip it and read the next.
            let mut r = iter.next()?;
            if iter.is_empty {
                r = iter.next()?;
            }
            let g = iter.next()?;
            let b = iter.next()?;
            Some(Color::Rgb([r, g, b]))
        }
        5 => Some(Color::Index(iter.next()?)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sgr_csi(params: &[u8]) -> Csi {
        Csi {
            intermediate: None,
            private_marker: None,
            final_byte: b'm',
            params: params.to_vec(),
        }
    }

    fn grapheme_at(s: &Screen, col: usize, row: usize) -> String {
        s.read_cell(col, row).unwrap().char.grapheme().to_string()
    }

    #[test]
    fn print_advances_cursor_and_writes_cells() {
        let mut s = Screen::new(4, 2);
        s.print("a", 1, false);
        s.print("b", 1, false);
        assert_eq!(grapheme_at(&s, 0, 0), "a");
        assert_eq!(grapheme_at(&s, 1, 0), "b");
        assert_eq!(s.cursor.col, 2);
        assert_eq!(s.cursor.row, 0);
    }

    #[test]
    fn print_writes_wide_grapheme() {
        let mut s = Screen::new(4, 1);
        s.print("漢", 2, false);
        assert_eq!(grapheme_at(&s, 0, 0), "漢");
        assert_eq!(s.read_cell(0, 0).unwrap().char.width, 2);
        // A width-2 glyph advances the cursor by two columns.
        assert_eq!(s.cursor.col, 2);
    }

    #[test]
    fn cursor_down_then_left() {
        let mut s = Screen::new(4, 3);
        s.cursor.col = 2;
        s.cursor.row = 0;
        s.cursor_down(1);
        assert_eq!(s.cursor.row, 1);
        s.cursor_left(1);
        assert_eq!(s.cursor.col, 1);
    }

    #[test]
    fn cursor_down_clamps_to_region_bottom() {
        let mut s = Screen::new(4, 3);
        // Region bottom is row 2; a large step clamps there.
        s.cursor_down(100);
        assert_eq!(s.cursor.row, 2);
    }

    #[test]
    fn index_at_region_bottom_scrolls_up() {
        let mut s = Screen::new(2, 2);
        // Row 0 = "ab".
        s.print("a", 1, false);
        s.print("b", 1, false);
        // Row 1 = "cd".
        s.cursor.col = 0;
        s.cursor.row = 1;
        s.print("c", 1, false);
        s.print("d", 1, false);

        // At the bottom row, index scrolls the region up by one line.
        s.cursor.col = 0;
        s.cursor.row = 1;
        s.index();

        assert_eq!(grapheme_at(&s, 0, 0), "c");
        assert_eq!(grapheme_at(&s, 1, 0), "d");
        assert_eq!(grapheme_at(&s, 0, 1), " ");
        assert_eq!(grapheme_at(&s, 1, 1), " ");
    }

    #[test]
    fn erase_line_and_below() {
        let mut s = Screen::new(3, 2);
        // Fill both rows.
        for ch in ["a", "b", "c"] {
            s.print(ch, 1, false);
        }
        s.cursor.col = 0;
        s.cursor.row = 1;
        for ch in ["d", "e", "f"] {
            s.print(ch, 1, false);
        }

        // Erase row 0.
        s.cursor.col = 0;
        s.cursor.row = 0;
        s.erase_line();
        assert_eq!(grapheme_at(&s, 0, 0), " ");
        assert_eq!(grapheme_at(&s, 2, 0), " ");
        // Row 1 untouched.
        assert_eq!(grapheme_at(&s, 0, 1), "d");

        // erase_below from (1, 1) clears the rest of row 1 (and anything after).
        s.cursor.col = 1;
        s.cursor.row = 1;
        s.erase_below();
        assert_eq!(grapheme_at(&s, 0, 1), "d");
        assert_eq!(grapheme_at(&s, 1, 1), " ");
        assert_eq!(grapheme_at(&s, 2, 1), " ");
    }

    #[test]
    fn insert_and_delete_line() {
        let mut s = Screen::new(2, 3);
        // Rows: "ab" / "cd" / "ef".
        for (row, pair) in [["a", "b"], ["c", "d"], ["e", "f"]].iter().enumerate() {
            s.cursor.col = 0;
            s.cursor.row = u16::try_from(row).unwrap();
            for ch in pair {
                s.print(ch, 1, false);
            }
        }

        // delete_line at row 0 pulls rows up: "cd" / "ef" / blank.
        s.cursor.col = 0;
        s.cursor.row = 0;
        s.delete_line(1);
        assert_eq!(grapheme_at(&s, 0, 0), "c");
        assert_eq!(grapheme_at(&s, 0, 1), "e");
        assert_eq!(grapheme_at(&s, 0, 2), " ");

        // insert_line at row 0 pushes rows down and blanks row 0.
        s.cursor.col = 0;
        s.cursor.row = 0;
        s.insert_line(1);
        assert_eq!(grapheme_at(&s, 0, 0), " ");
        assert_eq!(grapheme_at(&s, 0, 1), "c");
        assert_eq!(grapheme_at(&s, 0, 2), "e");
    }

    #[test]
    fn insert_line_terminates_on_bottom_row_cursor() {
        // Untrusted child output can place the cursor on the region's bottom
        // row, where `adjusted_n` is 0. With `top == 0` the bottom-up shift used
        // to spin forever (see the NOTE in `insert_line`). The defensive break
        // guarantees it returns, so reaching the assert at all proves
        // termination. `scroll_down` on a 1-row top region reaches the same
        // degenerate path, so this covers it too.
        let mut s = Screen::new(2, 3);
        s.cursor.col = 0;
        s.cursor.row = 2; // region bottom, with region top == 0
        s.insert_line(1);
        assert_eq!(s.buf.len(), 6);
    }

    #[test]
    fn delete_characters_shifts_row_left() {
        let mut s = Screen::new(4, 1);
        for ch in ["a", "b", "c", "d"] {
            s.print(ch, 1, false);
        }
        // Delete the char at col 1 ('b'); the rest shift left, last cell blanks.
        s.cursor.col = 1;
        s.cursor.row = 0;
        s.delete_characters(1);
        assert_eq!(grapheme_at(&s, 0, 0), "a");
        assert_eq!(grapheme_at(&s, 1, 0), "c");
        assert_eq!(grapheme_at(&s, 2, 0), "d");
        assert_eq!(grapheme_at(&s, 3, 0), " ");
    }

    #[test]
    fn sgr_attributes_and_reset() {
        let mut s = Screen::new(2, 1);
        s.sgr(&sgr_csi(b"1"));
        assert!(s.cursor.style.bold);
        s.sgr(&sgr_csi(b"3"));
        assert!(s.cursor.style.italic);
        // Empty params reset the style.
        s.sgr(&sgr_csi(b""));
        assert_eq!(s.cursor.style, Style::default());
    }

    #[test]
    fn sgr_indexed_colors() {
        let mut s = Screen::new(2, 1);
        s.sgr(&sgr_csi(b"31"));
        assert_eq!(s.cursor.style.fg, Color::Index(1));
        s.sgr(&sgr_csi(b"44"));
        assert_eq!(s.cursor.style.bg, Color::Index(4));
        // Bright foreground.
        s.sgr(&sgr_csi(b"91"));
        assert_eq!(s.cursor.style.fg, Color::Index(9));
    }

    #[test]
    fn sgr_256_color() {
        let mut s = Screen::new(2, 1);
        s.sgr(&sgr_csi(b"38;5;9"));
        assert_eq!(s.cursor.style.fg, Color::Index(9));
        s.sgr(&sgr_csi(b"48;5;200"));
        assert_eq!(s.cursor.style.bg, Color::Index(200));
    }

    #[test]
    fn sgr_rgb_color() {
        let mut s = Screen::new(2, 1);
        s.sgr(&sgr_csi(b"38;2;10;20;30"));
        assert_eq!(s.cursor.style.fg, Color::Rgb([10, 20, 30]));
        s.sgr(&sgr_csi(b"48;2;1;2;3"));
        assert_eq!(s.cursor.style.bg, Color::Rgb([1, 2, 3]));
    }

    #[test]
    fn sgr_rgb_with_sub_params_and_empty() {
        // The colon form with an empty color-space id: 38:2::r:g:b.
        let mut s = Screen::new(2, 1);
        s.sgr(&sgr_csi(b"38:2::10:20:30"));
        assert_eq!(s.cursor.style.fg, Color::Rgb([10, 20, 30]));
    }

    #[test]
    fn sgr_underline_styles() {
        let mut s = Screen::new(2, 1);
        s.sgr(&sgr_csi(b"4"));
        assert_eq!(s.cursor.style.ul_style, Underline::Single);
        // Curly underline via the sub-parameter form.
        s.sgr(&sgr_csi(b"4:3"));
        assert_eq!(s.cursor.style.ul_style, Underline::Curly);
        // Double underline via SGR 21.
        s.sgr(&sgr_csi(b"21"));
        assert_eq!(s.cursor.style.ul_style, Underline::Double);
        // Off.
        s.sgr(&sgr_csi(b"24"));
        assert_eq!(s.cursor.style.ul_style, Underline::Off);
    }

    #[test]
    fn copy_to_copies_only_dirty_cells() {
        let mut src = Screen::new(3, 1);
        let mut dst = Screen::new(3, 1);

        // First copy clears all of src's dirty flags.
        src.copy_to(&mut dst);

        // Corrupt a dst cell that src will not touch.
        dst.buf[1].char = "Z".to_string();

        // Only cell 0 is dirtied.
        src.print("x", 1, false);
        src.copy_to(&mut dst);

        assert_eq!(dst.buf[0].char, "x");
        // Cell 1 was not dirty in src, so copy_to left the corruption in place.
        assert_eq!(dst.buf[1].char, "Z");
        // The cursor is copied across.
        assert_eq!(dst.cursor.col, src.cursor.col);
    }

    #[test]
    fn erase_retains_capacity() {
        let mut cell = Cell {
            char: "wide-ish".to_string(),
            ..Cell::default()
        };
        let cap_before = cell.char.capacity();
        cell.erase(Color::Default);
        assert_eq!(cell.char, " ");
        // The buffer is cleared but never fully freed.
        assert!(cell.char.capacity() >= cap_before);
    }
}
