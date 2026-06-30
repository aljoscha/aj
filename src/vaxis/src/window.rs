//! A cheap clipped view into a [`crate::screen::Screen`] that also hosts the
//! text print and wrap engine.
//!
//! # Borrow model
//!
//! Upstream `Window` is a small value type holding a raw `*Screen`. Windows are
//! created freely (every `child` is a fresh value), they never own the screen,
//! and they all mutate the one shared screen through that pointer.
//!
//! We reproduce that shape without `unsafe` by parameterizing `Window` over the
//! screen borrow: it holds `&'s RefCell<Screen>`. This keeps `Window` `Copy`
//! and cheap to clone, lets `child` mint new windows that target the same
//! screen, and routes every mutation (`write_cell`, `fill`, `scroll`,
//! `show_cursor`, ...) through the shared cell via the `RefCell`.
//!
//! The alternative, threading `&mut Screen` through every method, was rejected:
//! it cannot express "create several child windows, all of which can write"
//! that the widget layers rely on, and `child` itself draws borders so it would
//! need the mutable borrow too.
//!
//! Borrow discipline: every method takes the `RefCell` borrow for the shortest
//! possible span and never holds one across a call that takes another. Reads
//! (`gwidth`, `read_cell`) use `borrow`, writes use `borrow_mut`, and compound
//! operations (`scroll`) drop their `borrow_mut` before calling back into
//! `child`/`clear`. With that discipline the runtime borrow checks never fail.

use std::cell::RefCell;

use crate::cell::{Cell, Character, CursorShape, Segment, Style};
use crate::mouse::Mouse;
use crate::screen::Screen;
use crate::unicode::grapheme_iterator;

/// A clipped, offset view into a shared [`Screen`].
///
/// Offsets are `i32` (upstream `i17`): a child can sit left of or above its
/// parent, so origins go negative and the clipping math must stay signed.
#[derive(Debug, Clone, Copy)]
pub struct Window<'s> {
    /// Absolute horizontal offset from the screen origin.
    pub x_off: i32,
    /// Absolute vertical offset from the screen origin.
    pub y_off: i32,
    /// Relative horizontal offset from the parent. Only accumulates while
    /// negative, so the window clips correctly against the parent's left edge.
    pub parent_x_off: i32,
    /// Relative vertical offset from the parent. Only accumulates while
    /// negative, so the window clips correctly against the parent's top edge.
    pub parent_y_off: i32,
    /// Window width, clamped so it never exceeds the screen.
    pub width: u16,
    /// Window height, clamped so it never exceeds the screen.
    pub height: u16,

    pub screen: &'s RefCell<Screen>,
}

/// Clamps a signed value into `u16` range, saturating at both ends.
fn clamp_u16(v: i32) -> u16 {
    u16::try_from(v.clamp(0, i32::from(u16::MAX))).unwrap_or(0)
}

/// Clamps a signed value into `usize` range, flooring negatives at 0.
fn clamp_usize(v: i32) -> usize {
    usize::try_from(v.max(0)).unwrap_or(0)
}

impl<'s> Window<'s> {
    /// Creates a child with offsets relative to this window and a size clamped
    /// to the remaining space. Children do not retain a reference to their
    /// parent and are unaware of resizes.
    fn init_child(
        &self,
        x_off: i32,
        y_off: i32,
        maybe_width: Option<u16>,
        maybe_height: Option<u16>,
    ) -> Window<'s> {
        let max_height = (i32::from(self.height) - y_off).max(0);
        let max_width = (i32::from(self.width) - x_off).max(0);
        let width = maybe_width.map_or(max_width, i32::from);
        let height = maybe_height.map_or(max_height, i32::from);

        Window {
            x_off: x_off + self.x_off,
            y_off: y_off + self.y_off,
            parent_x_off: (self.parent_x_off + x_off).min(0),
            parent_y_off: (self.parent_y_off + y_off).min(0),
            width: clamp_u16(width.min(max_width)),
            height: clamp_u16(height.min(max_height)),
            screen: self.screen,
        }
    }

    /// Creates a child window, optionally drawing a border into it.
    ///
    /// The requested width and height include any border. The returned window
    /// is the interior: when a border edge is drawn the interior is inset by
    /// one cell on that edge.
    pub fn child(&self, opts: ChildOptions) -> Window<'s> {
        let result = self.init_child(opts.x_off, opts.y_off, opts.width, opts.height);

        let single_rounded: [&str; 6] = ["╭", "─", "╮", "│", "╯", "╰"];
        let single_square: [&str; 6] = ["┌", "─", "┐", "│", "┘", "└"];
        let glyphs: [&str; 6] = match &opts.border.glyphs {
            Glyphs::SingleRounded => single_rounded,
            Glyphs::SingleSquare => single_square,
            Glyphs::Custom(c) => [
                c[0].as_str(),
                c[1].as_str(),
                c[2].as_str(),
                c[3].as_str(),
                c[4].as_str(),
                c[5].as_str(),
            ],
        };

        let top_left = Character::new(glyphs[0], 1);
        let horizontal = Character::new(glyphs[1], 1);
        let top_right = Character::new(glyphs[2], 1);
        let vertical = Character::new(glyphs[3], 1);
        let bottom_right = Character::new(glyphs[4], 1);
        let bottom_left = Character::new(glyphs[5], 1);
        let style = opts.border.style;

        let h = result.height;
        let w = result.width;

        let loc = match opts.border.location {
            BorderWhere::None => return result,
            BorderWhere::All => Locations {
                top: true,
                bottom: true,
                right: true,
                left: true,
            },
            BorderWhere::Bottom => Locations {
                bottom: true,
                ..Locations::default()
            },
            BorderWhere::Right => Locations {
                right: true,
                ..Locations::default()
            },
            BorderWhere::Left => Locations {
                left: true,
                ..Locations::default()
            },
            BorderWhere::Top => Locations {
                top: true,
                ..Locations::default()
            },
            BorderWhere::Other(loc) => loc,
        };

        if loc.top {
            for i in 0..w {
                result.write_cell(
                    i,
                    0,
                    Cell {
                        char: horizontal.clone(),
                        style,
                        ..Cell::default()
                    },
                );
            }
        }
        if loc.bottom {
            for i in 0..w {
                result.write_cell(
                    i,
                    h.saturating_sub(1),
                    Cell {
                        char: horizontal.clone(),
                        style,
                        ..Cell::default()
                    },
                );
            }
        }
        if loc.left {
            for i in 0..h {
                result.write_cell(
                    0,
                    i,
                    Cell {
                        char: vertical.clone(),
                        style,
                        ..Cell::default()
                    },
                );
            }
        }
        if loc.right {
            for i in 0..h {
                result.write_cell(
                    w.saturating_sub(1),
                    i,
                    Cell {
                        char: vertical.clone(),
                        style,
                        ..Cell::default()
                    },
                );
            }
        }
        if loc.top && loc.left {
            result.write_cell(
                0,
                0,
                Cell {
                    char: top_left,
                    style,
                    ..Cell::default()
                },
            );
        }
        if loc.top && loc.right {
            result.write_cell(
                w.saturating_sub(1),
                0,
                Cell {
                    char: top_right,
                    style,
                    ..Cell::default()
                },
            );
        }
        if loc.bottom && loc.left {
            result.write_cell(
                0,
                h.saturating_sub(1),
                Cell {
                    char: bottom_left,
                    style,
                    ..Cell::default()
                },
            );
        }
        if loc.bottom && loc.right {
            result.write_cell(
                w.saturating_sub(1),
                h.saturating_sub(1),
                Cell {
                    char: bottom_right,
                    style,
                    ..Cell::default()
                },
            );
        }

        let x_off: u16 = u16::from(loc.left);
        let y_off: u16 = u16::from(loc.top);
        let h_delt: u16 = u16::from(loc.bottom);
        let w_delt: u16 = u16::from(loc.right);
        let h_ch: u16 = h.saturating_sub(y_off).saturating_sub(h_delt);
        let w_ch: u16 = w.saturating_sub(x_off).saturating_sub(w_delt);
        result.init_child(i32::from(x_off), i32::from(y_off), Some(w_ch), Some(h_ch))
    }

    /// Writes `cell` at the window-local 0-indexed `(col, row)`. Positions
    /// outside the window or clipped off the parent are silently ignored.
    pub fn write_cell(&self, col: u16, row: u16, cell: Cell) {
        let coli = i32::from(col);
        let rowi = i32::from(row);
        if i32::from(self.height) <= rowi
            || i32::from(self.width) <= coli
            || self.x_off + coli < 0
            || self.y_off + rowi < 0
            || self.parent_x_off + coli < 0
            || self.parent_y_off + rowi < 0
        {
            return;
        }
        let (Ok(sc), Ok(sr)) = (
            u16::try_from(self.x_off + coli),
            u16::try_from(self.y_off + rowi),
        ) else {
            return;
        };
        self.screen.borrow_mut().write_cell(sc, sr, cell);
    }

    /// Reads the cell at the window-local 0-indexed `(col, row)`, or `None`
    /// when outside the window or clipped off the parent.
    pub fn read_cell(&self, col: u16, row: u16) -> Option<Cell> {
        let coli = i32::from(col);
        let rowi = i32::from(row);
        if i32::from(self.height) <= rowi
            || i32::from(self.width) <= coli
            || self.x_off + coli < 0
            || self.y_off + rowi < 0
            || self.parent_x_off + coli < 0
            || self.parent_y_off + rowi < 0
        {
            return None;
        }
        let sc = u16::try_from(self.x_off + coli).ok()?;
        let sr = u16::try_from(self.y_off + rowi).ok()?;
        self.screen.borrow().read_cell(sc, sr)
    }

    /// Fills the window with the default cell.
    pub fn clear(&self) {
        self.fill(Cell {
            default: true,
            ..Cell::default()
        });
    }

    /// Returns the display width of `str`, measured with the screen's width
    /// method.
    pub fn gwidth(&self, s: &str) -> u16 {
        crate::gwidth::gwidth(s, self.screen.borrow().width_method)
    }

    /// Pixel width of the underlying screen.
    pub fn screen_width_pix(&self) -> u16 {
        self.screen.borrow().width_pix
    }

    /// Pixel height of the underlying screen.
    pub fn screen_height_pix(&self) -> u16 {
        self.screen.borrow().height_pix
    }

    /// Cell width of the underlying screen.
    pub fn screen_width(&self) -> u16 {
        self.screen.borrow().width
    }

    /// Cell height of the underlying screen.
    pub fn screen_height(&self) -> u16 {
        self.screen.borrow().height
    }

    /// Fills the window with `cell`.
    pub fn fill(&self, cell: Cell) {
        let mut screen = self.screen.borrow_mut();
        if self.x_off + i32::from(self.width) < 0
            || self.y_off + i32::from(self.height) < 0
            || i32::from(screen.width) < self.x_off
            || i32::from(screen.height) < self.y_off
        {
            return;
        }
        let buf_len = screen.buf.len();
        let first_row = clamp_usize(self.y_off);
        if self.x_off == 0 && self.width == screen.width {
            // Full-width window: the cells are contiguous, fill in one span.
            let start = (first_row * usize::from(self.width)).min(buf_len);
            let end = (start + usize::from(self.height) * usize::from(self.width)).min(buf_len);
            screen.buf[start..end].fill(cell);
        } else {
            // Non-contiguous: fill row by row, clamping each span to the row's
            // right edge so it never bleeds into the next row.
            let screen_width = usize::from(screen.width);
            let first_col = clamp_usize(self.x_off);
            let last_row =
                clamp_usize((i32::from(self.height) + self.y_off).min(i32::from(screen.height)));
            let mut row = first_row;
            while row < last_row {
                let start = (first_col + row * screen_width).min(buf_len);
                let remaining = screen_width.saturating_sub(first_col);
                let end = (start + usize::from(self.width))
                    .min(start + remaining)
                    .min(buf_len);
                screen.buf[start..end].fill(cell.clone());
                row += 1;
            }
        }
    }

    /// Hides the cursor.
    pub fn hide_cursor(&self) {
        self.screen.borrow_mut().cursor_vis = false;
    }

    /// Shows the cursor at the window-local 0-indexed `(col, row)`. Positions
    /// outside the window are ignored.
    pub fn show_cursor(&self, col: u16, row: u16) {
        let coli = i32::from(col);
        let rowi = i32::from(row);
        if self.x_off + coli < 0 || self.y_off + rowi < 0 || row >= self.height || col >= self.width
        {
            return;
        }
        let mut screen = self.screen.borrow_mut();
        screen.cursor_vis = true;
        screen.cursor.row = clamp_u16(self.y_off + rowi);
        screen.cursor.col = clamp_u16(self.x_off + coli);
    }

    /// Sets the cursor shape on the underlying screen.
    pub fn set_cursor_shape(&self, shape: CursorShape) {
        self.screen.borrow_mut().cursor_shape = shape;
    }

    /// Prints `segments` to the window, laying out text per `opts`.
    ///
    /// Returns the cursor position after the printed text and whether it
    /// overflowed the window with the given wrap strategy. When
    /// `opts.commit` is false nothing is written and only the measurement is
    /// returned.
    pub fn print(&self, segments: &[Segment], opts: PrintOptions) -> PrintResult {
        let mut row = opts.row_offset;
        match opts.wrap {
            Wrap::Grapheme => {
                let mut col = opts.col_offset;
                let mut overflow = false;
                'outer: for segment in segments {
                    for grapheme in grapheme_iterator(&segment.text) {
                        if col >= self.width {
                            row += 1;
                            col = 0;
                        }
                        if row >= self.height {
                            overflow = true;
                            break 'outer;
                        }
                        let s = grapheme.bytes(&segment.text);
                        if s == "\n" {
                            row = row.saturating_add(1);
                            col = 0;
                            continue;
                        }
                        let w = self.gwidth(s);
                        if w == 0 {
                            continue;
                        }
                        if opts.commit {
                            self.write_cell(
                                col,
                                row,
                                Cell {
                                    char: Character::new(s, u8::try_from(w).unwrap_or(u8::MAX)),
                                    style: segment.style,
                                    link: segment.link.clone(),
                                    wrapped: col + w >= self.width,
                                    ..Cell::default()
                                },
                            );
                        }
                        col += w;
                    }
                }
                // Runs whether or not we overflowed: this is the post-loop
                // wrap upstream applies after the labeled block.
                if col >= self.width {
                    row += 1;
                    col = 0;
                }
                PrintResult { row, col, overflow }
            }
            Wrap::Word => self.print_word(segments, row, opts),
            Wrap::None => {
                let mut col = opts.col_offset;
                let mut overflow = false;
                'outer: for segment in segments {
                    for grapheme in grapheme_iterator(&segment.text) {
                        if col >= self.width {
                            overflow = true;
                            break 'outer;
                        }
                        let s = grapheme.bytes(&segment.text);
                        if s == "\n" {
                            overflow = true;
                            break 'outer;
                        }
                        let w = self.gwidth(s);
                        if w == 0 {
                            continue;
                        }
                        if opts.commit {
                            self.write_cell(
                                col,
                                row,
                                Cell {
                                    char: Character::new(s, u8::try_from(w).unwrap_or(u8::MAX)),
                                    style: segment.style,
                                    link: segment.link.clone(),
                                    ..Cell::default()
                                },
                            );
                        }
                        col = col.saturating_add(w);
                    }
                }
                PrintResult { row, col, overflow }
            }
        }
    }

    /// Word-wrap branch of [`Window::print`], split out to keep `print`
    /// readable.
    fn print_word(&self, segments: &[Segment], mut row: u16, opts: PrintOptions) -> PrintResult {
        let mut col = opts.col_offset;
        let mut overflow = false;
        // Tracks whether the previous word's grapheme just soft-wrapped to a
        // new row. A leading run of whitespace after a soft wrap is swallowed.
        let mut soft_wrapped = false;
        'outer: for segment in segments {
            let mut line_iter = LineIterator::new(&segment.text);
            while let Some(line) = line_iter.next() {
                let mut broke_outer = false;
                let mut tokens = WhitespaceTokenizer {
                    buf: line,
                    index: 0,
                };
                while let Some(token) = tokens.next() {
                    match token {
                        Token::Whitespace(len) => {
                            if soft_wrapped {
                                continue;
                            }
                            for _ in 0..len {
                                if col >= self.width {
                                    col = 0;
                                    row += 1;
                                    break;
                                }
                                if opts.commit {
                                    self.write_cell(
                                        col,
                                        row,
                                        Cell {
                                            char: Character::new(" ", 1),
                                            style: segment.style,
                                            link: segment.link.clone(),
                                            ..Cell::default()
                                        },
                                    );
                                }
                                col += 1;
                            }
                        }
                        Token::Word(word) => {
                            let width = self.gwidth(word);
                            if width + col > self.width && width < self.width {
                                row += 1;
                                col = 0;
                            }
                            for grapheme in grapheme_iterator(word) {
                                soft_wrapped = false;
                                if row >= self.height {
                                    overflow = true;
                                    broke_outer = true;
                                    break;
                                }
                                let s = grapheme.bytes(word);
                                let w = self.gwidth(s);
                                if opts.commit {
                                    self.write_cell(
                                        col,
                                        row,
                                        Cell {
                                            char: Character::new(
                                                s,
                                                u8::try_from(w).unwrap_or(u8::MAX),
                                            ),
                                            style: segment.style,
                                            link: segment.link.clone(),
                                            ..Cell::default()
                                        },
                                    );
                                }
                                col += w;
                                if col >= self.width {
                                    row += 1;
                                    col = 0;
                                    soft_wrapped = true;
                                }
                            }
                            if broke_outer {
                                break;
                            }
                        }
                    }
                }
                // Upstream runs this as a `defer` at the end of each line body,
                // so it fires on the overflow break too. A line that ended at a
                // real linebreak advances to the next row.
                if line_iter.has_break {
                    soft_wrapped = false;
                    row += 1;
                    col = 0;
                }
                if broke_outer {
                    break 'outer;
                }
            }
        }
        PrintResult { row, col, overflow }
    }

    /// Prints a single segment. Shortcut for `print(&[segment], opts)`.
    pub fn print_segment(&self, segment: Segment, opts: PrintOptions) -> PrintResult {
        self.print(&[segment], opts)
    }

    /// Scrolls the window contents up by `n` rows, clearing the freed rows at
    /// the bottom.
    pub fn scroll(&self, n: u16) {
        if n > self.height {
            return;
        }
        {
            let mut screen = self.screen.borrow_mut();
            let screen_width = usize::from(screen.width);
            let first_col = clamp_usize(self.x_off);
            let width = usize::from(self.width);
            let buf_len = screen.buf.len();
            let limit = self.height - n;
            let mut row = clamp_u16(self.y_off);
            while row < limit {
                let dst_start = usize::from(row) * screen_width + first_col;
                let src_start = (usize::from(row) + usize::from(n)) * screen_width + first_col;
                // dst_end <= src_start always (dst row is above src row), so the
                // ranges are disjoint and split_at_mut can hand out both.
                if src_start + width > buf_len || dst_start + width > buf_len {
                    break;
                }
                let (left, right) = screen.buf.split_at_mut(src_start);
                left[dst_start..dst_start + width].clone_from_slice(&right[..width]);
                row += 1;
            }
        }
        let last_row = self.child(ChildOptions {
            y_off: i32::from(self.height - n),
            ..ChildOptions::default()
        });
        last_row.clear();
    }

    /// Returns `mouse` if its position falls within the window, else `None`.
    pub fn has_mouse(&self, mouse: Option<Mouse>) -> Option<Mouse> {
        let event = mouse?;
        let col = i32::from(event.col);
        let row = i32::from(event.row);
        if col >= self.x_off
            && col < self.x_off + i32::from(self.width)
            && row >= self.y_off
            && row < self.y_off + i32::from(self.height)
        {
            Some(event)
        } else {
            None
        }
    }
}

/// Options for [`Window::child`].
#[derive(Debug, Clone, Default)]
pub struct ChildOptions {
    pub x_off: i32,
    pub y_off: i32,
    /// Width of the resulting child, including any border.
    pub width: Option<u16>,
    /// Height of the resulting child, including any border.
    pub height: Option<u16>,
    pub border: BorderOptions,
}

/// Where a child window's border is drawn.
///
/// Named `location` on [`BorderOptions`] because `where` is a Rust keyword.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BorderWhere {
    #[default]
    None,
    All,
    Top,
    Right,
    Bottom,
    Left,
    Other(Locations),
}

/// Which edges of a border to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Locations {
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
    pub left: bool,
}

/// The glyph set used to draw a border.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Glyphs {
    #[default]
    SingleRounded,
    SingleSquare,
    /// Custom border glyphs, each one cell wide, indexed as: 0 top-left,
    /// 1 horizontal, 2 top-right, 3 vertical, 4 bottom-right, 5 bottom-left.
    Custom([String; 6]),
}

/// Border styling and placement for [`Window::child`].
#[derive(Debug, Clone, Default)]
pub struct BorderOptions {
    pub style: Style,
    pub location: BorderWhere,
    pub glyphs: Glyphs,
}

/// Wrap strategy for [`Window::print`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Wrap {
    /// Wrap at grapheme boundaries.
    #[default]
    Grapheme,
    /// Wrap at word boundaries.
    Word,
    /// Stop printing after one line.
    None,
}

/// Options for [`Window::print`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrintOptions {
    /// Vertical offset to start printing at.
    pub row_offset: u16,
    /// Horizontal offset to start printing at.
    pub col_offset: u16,
    pub wrap: Wrap,
    /// When true, the printed cells are committed to the screen. When false,
    /// nothing is written and only the resulting [`PrintResult`] is computed.
    pub commit: bool,
}

impl Default for PrintOptions {
    fn default() -> Self {
        Self {
            row_offset: 0,
            col_offset: 0,
            wrap: Wrap::Grapheme,
            commit: true,
        }
    }
}

/// The cursor position after a [`Window::print`] plus whether the text
/// overflowed the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrintResult {
    pub col: u16,
    pub row: u16,
    pub overflow: bool,
}

/// Splits a string into lines on `\r`, `\n`, or `\r\n`.
///
/// `has_break` reports whether the most recently yielded line was terminated by
/// a linebreak. It starts true and is cleared only when the very first line
/// runs to the end of the buffer without one, which the word-wrap engine reads
/// to decide whether to advance a row after a line.
struct LineIterator<'a> {
    buf: &'a str,
    index: usize,
    has_break: bool,
}

impl<'a> LineIterator<'a> {
    fn new(buf: &'a str) -> Self {
        Self {
            buf,
            index: 0,
            has_break: true,
        }
    }

    fn consume_cr(&mut self) {
        if self.index >= self.buf.len() {
            return;
        }
        if self.buf.as_bytes()[self.index] == b'\r' {
            self.index += 1;
        }
    }

    fn consume_lf(&mut self) {
        if self.index >= self.buf.len() {
            return;
        }
        if self.buf.as_bytes()[self.index] == b'\n' {
            self.index += 1;
        }
    }
}

impl<'a> Iterator for LineIterator<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        if self.index >= self.buf.len() {
            return None;
        }
        let start = self.index;
        let bytes = self.buf.as_bytes();
        // `\r` and `\n` are ASCII, so the byte position is always a valid char
        // boundary and the slices below are sound.
        let found = bytes[self.index..]
            .iter()
            .position(|&b| b == b'\r' || b == b'\n')
            .map(|p| self.index + p);
        match found {
            None => {
                if start == 0 {
                    self.has_break = false;
                }
                self.index = self.buf.len();
                Some(&self.buf[start..])
            }
            Some(end) => {
                self.index = end;
                self.consume_cr();
                self.consume_lf();
                Some(&self.buf[start..end])
            }
        }
    }
}

/// A token yielded by [`WhitespaceTokenizer`].
enum Token<'a> {
    /// A run of whitespace, measured in cells (space = 1, tab = 8).
    Whitespace(usize),
    Word(&'a str),
}

/// Splits a line into alternating runs of whitespace and non-whitespace.
struct WhitespaceTokenizer<'a> {
    buf: &'a str,
    index: usize,
}

impl<'a> Iterator for WhitespaceTokenizer<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        if self.index >= self.buf.len() {
            return None;
        }
        let bytes = self.buf.as_bytes();
        let first = bytes[self.index];
        if first == b' ' || first == b'\t' {
            let mut len = 0usize;
            while self.index < self.buf.len() {
                match bytes[self.index] {
                    b' ' => len += 1,
                    b'\t' => len += 8,
                    _ => break,
                }
                self.index += 1;
            }
            Some(Token::Whitespace(len))
        } else {
            let start = self.index;
            // Multibyte UTF-8 bytes are all >= 0x80, never space or tab, so we
            // advance through whole characters and stop on a char boundary.
            while self.index < self.buf.len() {
                match bytes[self.index] {
                    b' ' | b'\t' => break,
                    _ => self.index += 1,
                }
            }
            Some(Token::Word(&self.buf[start..self.index]))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway screen for the offset/size tests, which never touch it.
    fn empty_screen() -> RefCell<Screen> {
        RefCell::new(Screen::default())
    }

    #[test]
    fn window_size_set() {
        let screen = empty_screen();
        let parent = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 20,
            height: 20,
            screen: &screen,
        };
        let ch = parent.init_child(1, 1, None, None);
        assert_eq!(ch.width, 19);
        assert_eq!(ch.height, 19);
    }

    #[test]
    fn window_size_set_too_big() {
        let screen = empty_screen();
        let parent = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 20,
            height: 20,
            screen: &screen,
        };
        let ch = parent.init_child(0, 0, Some(21), Some(21));
        assert_eq!(ch.width, 20);
        assert_eq!(ch.height, 20);
    }

    #[test]
    fn window_size_set_too_big_with_offset() {
        let screen = empty_screen();
        let parent = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 20,
            height: 20,
            screen: &screen,
        };
        let ch = parent.init_child(10, 10, Some(21), Some(21));
        assert_eq!(ch.width, 10);
        assert_eq!(ch.height, 10);
    }

    #[test]
    fn window_size_nested_offsets() {
        let screen = empty_screen();
        let parent = Window {
            x_off: 1,
            y_off: 1,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 20,
            height: 20,
            screen: &screen,
        };
        let ch = parent.init_child(10, 10, Some(21), Some(21));
        assert_eq!(ch.x_off, 11);
        assert_eq!(ch.y_off, 11);
    }

    #[test]
    fn window_offsets() {
        let screen = empty_screen();
        let parent = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 20,
            height: 20,
            screen: &screen,
        };
        let ch = parent.init_child(10, 10, Some(21), Some(21));
        let ch2 = ch.init_child(-4, -4, None, None);
        // Reading ch2 at row 0 should be clipped off the parent's edge.
        assert!(ch2.read_cell(0, 0).is_none());
        // Writing the same clipped cell must not panic.
        ch2.write_cell(0, 0, Cell::default());
    }

    fn unicode_screen() -> RefCell<Screen> {
        RefCell::new(Screen {
            width_method: crate::gwidth::Method::Unicode,
            ..Screen::default()
        })
    }

    fn seg(text: &str) -> Segment {
        Segment {
            text: text.to_string(),
            ..Segment::default()
        }
    }

    #[test]
    fn print_grapheme() {
        let screen = unicode_screen();
        let win = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 4,
            height: 2,
            screen: &screen,
        };
        let opts = PrintOptions {
            commit: false,
            wrap: Wrap::Grapheme,
            ..PrintOptions::default()
        };

        let cases: &[(&str, u16, u16, bool)] = &[
            ("a", 1, 0, false),
            ("abcd", 0, 1, false),
            ("abcde", 1, 1, false),
            ("abcdefgh", 0, 2, false),
            ("abcdefghi", 0, 2, true),
        ];
        for &(text, col, row, overflow) in cases {
            let result = win.print(&[seg(text)], opts);
            assert_eq!(
                result,
                PrintResult { col, row, overflow },
                "grapheme case {text:?}"
            );
        }
    }

    #[test]
    fn print_word() {
        let screen = unicode_screen();
        let win = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 4,
            height: 2,
            screen: &screen,
        };
        let opts = PrintOptions {
            commit: false,
            wrap: Wrap::Word,
            ..PrintOptions::default()
        };

        // Single-segment cases: (text, col, row, overflow).
        let cases: &[(&str, u16, u16, bool)] = &[
            ("a", 1, 0, false),
            (" ", 1, 0, false),
            (" a", 2, 0, false),
            ("a b", 3, 0, false),
            ("a b c", 1, 1, false),
            ("hello", 1, 1, false),
            ("hi tim", 3, 1, false),
            ("hello tim", 0, 2, true),
            ("hello ti", 0, 2, false),
            ("he\n", 0, 1, false),
            ("he\n\n", 0, 2, false),
            ("not now", 3, 1, false),
            ("note now", 3, 1, false),
        ];
        for &(text, col, row, overflow) in cases {
            let result = win.print(&[seg(text)], opts);
            assert_eq!(
                result,
                PrintResult { col, row, overflow },
                "word case {text:?}"
            );
        }

        // Multi-segment cases.
        assert_eq!(
            win.print(&[seg("h"), seg("e")], opts),
            PrintResult {
                col: 2,
                row: 0,
                overflow: false
            },
            "word case [h, e]"
        );
        assert_eq!(
            win.print(&[seg("h"), seg("e"), seg("l"), seg("l"), seg("o")], opts),
            PrintResult {
                col: 1,
                row: 1,
                overflow: false
            },
            "word case [h, e, l, l, o]"
        );
        assert_eq!(
            win.print(&[seg("note"), seg(" now")], opts),
            PrintResult {
                col: 3,
                row: 1,
                overflow: false
            },
            "word case [note,  now]"
        );
        assert_eq!(
            win.print(&[seg("note "), seg("now")], opts),
            PrintResult {
                col: 3,
                row: 1,
                overflow: false
            },
            "word case [note , now]"
        );
    }
}
