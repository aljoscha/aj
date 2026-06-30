//! A scrolling viewport with an optional vertical scrollbar.
//!
//! [`ScrollView`] holds a [`Scroll`] offset that the application either drives
//! directly or through [`input`](ScrollView::input). Content is drawn through
//! [`write_cell`](ScrollView::write_cell), which translates content coordinates
//! into window coordinates and culls anything outside the visible region.
//!
//! NOTE: [`draw`](ScrollView::draw) must be called before any
//! [`write_cell`](ScrollView::write_cell): it clamps the scroll offset to the
//! content size and renders the scrollbar. Drawing cells against a stale scroll
//! offset would place them at the wrong position.

use crate::cell::{Cell, Character, Color, Style};
use crate::key::{Key, Modifiers};
use crate::widgets::scrollbar::Scrollbar;
use crate::window::{ChildOptions, Window};

/// The scroll offset, in content cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Scroll {
    pub x: usize,
    pub y: usize,
}

impl Scroll {
    /// Clamps the offset so it never scrolls past `(w, h)`.
    pub fn restrict_to(&mut self, w: usize, h: usize) {
        self.x = self.x.min(w);
        self.y = self.y.min(h);
    }
}

/// Styling for the optional vertical scrollbar.
pub struct VerticalScrollbar {
    /// Character drawn for the bar and its track.
    pub character: Character,
    /// Style of the bar itself.
    pub fg: Style,
    /// Style of the track behind the bar.
    pub bg: Style,
}

impl Default for VerticalScrollbar {
    fn default() -> VerticalScrollbar {
        VerticalScrollbar {
            character: Character::new("▐", 1),
            fg: Style::default(),
            bg: Style {
                fg: Color::Index(8),
                ..Style::default()
            },
        }
    }
}

/// A scrolling viewport.
pub struct ScrollView {
    pub scroll: Scroll,
    /// The scrollbar to draw, or `None` to omit it. Present by default.
    pub vertical_scrollbar: Option<VerticalScrollbar>,
}

impl Default for ScrollView {
    fn default() -> ScrollView {
        ScrollView {
            scroll: Scroll::default(),
            vertical_scrollbar: Some(VerticalScrollbar::default()),
        }
    }
}

/// The size of the scrollable content, in cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentSize {
    pub cols: usize,
    pub rows: usize,
}

/// A rectangular region of content, used to cull cells outside the viewport.
///
/// The half-open box spans columns `[x1, x2)` and rows `[y1, y2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundingBox {
    pub x1: usize,
    pub y1: usize,
    pub x2: usize,
    pub y2: usize,
}

impl BoundingBox {
    /// True if `row` sits above the box's top edge.
    pub fn below(&self, row: usize) -> bool {
        row < self.y1
    }

    /// True if `row` sits at or below the box's bottom edge.
    pub fn above(&self, row: usize) -> bool {
        row >= self.y2
    }

    /// True if `row` is within the box's vertical span.
    pub fn row_inside(&self, row: usize) -> bool {
        row >= self.y1 && row < self.y2
    }

    /// True if `col` is within the box's horizontal span.
    pub fn col_inside(&self, col: usize) -> bool {
        col >= self.x1 && col < self.x2
    }

    /// True if `(col, row)` is inside the box.
    pub fn inside(&self, col: usize, row: usize) -> bool {
        self.row_inside(row) && self.col_inside(col)
    }
}

impl ScrollView {
    /// Standard key bindings for scrolling.
    ///
    /// Arrows scroll one cell, shifted arrows and page keys scroll 32 cells,
    /// and Home/End jump to the top/bottom. Setting [`scroll`](Self::scroll)
    /// directly is also fine.
    pub fn input(&mut self, key: &Key) {
        if key.matches(Key::RIGHT, Modifiers::empty()) {
            self.scroll.x = self.scroll.x.saturating_add(1);
        } else if key.matches(Key::RIGHT, Modifiers::SHIFT) {
            self.scroll.x = self.scroll.x.saturating_add(32);
        } else if key.matches(Key::LEFT, Modifiers::empty()) {
            self.scroll.x = self.scroll.x.saturating_sub(1);
        } else if key.matches(Key::LEFT, Modifiers::SHIFT) {
            self.scroll.x = self.scroll.x.saturating_sub(32);
        } else if key.matches(Key::UP, Modifiers::empty()) {
            self.scroll.y = self.scroll.y.saturating_sub(1);
        } else if key.matches(Key::PAGE_UP, Modifiers::empty()) {
            self.scroll.y = self.scroll.y.saturating_sub(32);
        } else if key.matches(Key::DOWN, Modifiers::empty()) {
            self.scroll.y = self.scroll.y.saturating_add(1);
        } else if key.matches(Key::PAGE_DOWN, Modifiers::empty()) {
            self.scroll.y = self.scroll.y.saturating_add(32);
        } else if key.matches(Key::END, Modifiers::empty()) {
            self.scroll.y = usize::MAX;
        } else if key.matches(Key::HOME, Modifiers::empty()) {
            self.scroll.y = 0;
        }
    }

    /// Clamps the scroll offset to the content size and draws the scrollbar.
    ///
    /// Must run before any [`write_cell`](Self::write_cell) call this frame.
    pub fn draw(&mut self, parent: Window<'_>, content_size: ContentSize) {
        // Reserve a column for the scrollbar when one is present.
        let content_cols = if self.vertical_scrollbar.is_some() {
            content_size.cols.saturating_add(1)
        } else {
            content_size.cols
        };
        let max_scroll_x = content_cols.saturating_sub(usize::from(parent.width));
        let max_scroll_y = content_size.rows.saturating_sub(usize::from(parent.height));
        self.scroll.restrict_to(max_scroll_x, max_scroll_y);
        if let Some(opts) = &self.vertical_scrollbar {
            let vbar = Scrollbar {
                character: opts.character.clone(),
                style: opts.fg,
                total: content_size.rows,
                view_size: usize::from(parent.height),
                top: self.scroll.y,
            };
            let bg = parent.child(ChildOptions {
                x_off: i32::from(parent.width.saturating_sub(u16::from(opts.character.width))),
                width: Some(u16::from(opts.character.width)),
                height: Some(parent.height),
                ..ChildOptions::default()
            });
            bg.fill(Cell {
                char: opts.character.clone(),
                style: opts.bg,
                ..Cell::default()
            });
            vbar.draw(bg);
        }
    }

    /// The content region currently visible, useful for culling draws.
    pub fn bounds(&self, parent: Window<'_>) -> BoundingBox {
        let right_pad: usize = if self.vertical_scrollbar.is_some() {
            1
        } else {
            0
        };
        BoundingBox {
            x1: self.scroll.x,
            y1: self.scroll.y,
            x2: self
                .scroll
                .x
                .saturating_add(usize::from(parent.width))
                .saturating_sub(right_pad),
            y2: self.scroll.y.saturating_add(usize::from(parent.height)),
        }
    }

    /// Writes `cell` at content coordinates `(col, row)`, scrolled into view.
    ///
    /// Use this instead of [`Window::write_cell`] so the cell scrolls with the
    /// content. Cells outside the visible bounds are dropped.
    pub fn write_cell(&self, parent: Window<'_>, col: usize, row: usize, cell: Cell) {
        let b = self.bounds(parent);
        if !b.inside(col, row) {
            return;
        }
        let win = parent.child(ChildOptions {
            width: Some(u16::try_from(b.x2 - b.x1).unwrap_or(u16::MAX)),
            height: Some(u16::try_from(b.y2 - b.y1).unwrap_or(u16::MAX)),
            ..ChildOptions::default()
        });
        win.write_cell(
            u16::try_from(col.saturating_sub(self.scroll.x)).unwrap_or(u16::MAX),
            u16::try_from(row.saturating_sub(self.scroll.y)).unwrap_or(u16::MAX),
            cell,
        );
    }

    /// Reads the content cell at `(col, row)`, scrolled into view, or `None`
    /// when it falls outside the visible bounds.
    ///
    /// NOTE: Upstream's out-of-bounds branch is `if (!b.inside(...)) return;`,
    /// a bare `return` from a function declared to return `?Cell`. That is a
    /// latent bug: it relies on the bare return coercing to `null` rather than
    /// the intended explicit `null`. We reproduce the intended observable
    /// behavior here by returning `None`, which is the only sound translation.
    pub fn read_cell(&self, parent: Window<'_>, col: usize, row: usize) -> Option<Cell> {
        let b = self.bounds(parent);
        if !b.inside(col, row) {
            return None;
        }
        let win = parent.child(ChildOptions {
            width: Some(u16::try_from(b.x2 - b.x1).unwrap_or(u16::MAX)),
            height: Some(u16::try_from(b.y2 - b.y1).unwrap_or(u16::MAX)),
            ..ChildOptions::default()
        });
        win.read_cell(
            u16::try_from(col.saturating_sub(self.scroll.x)).unwrap_or(u16::MAX),
            u16::try_from(row.saturating_sub(self.scroll.y)).unwrap_or(u16::MAX),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::screen::Screen;

    fn screen() -> RefCell<Screen> {
        RefCell::new(Screen::new(crate::Winsize {
            rows: 10,
            cols: 10,
            x_pixel: 0,
            y_pixel: 0,
        }))
    }

    fn win(screen: &RefCell<Screen>) -> Window<'_> {
        Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 10,
            height: 10,
            screen,
        }
    }

    #[test]
    fn restrict_to_clamps_both_axes() {
        let mut scroll = Scroll { x: 100, y: 100 };
        scroll.restrict_to(5, 7);
        assert_eq!(scroll, Scroll { x: 5, y: 7 });
    }

    #[test]
    fn input_moves_scroll() {
        let mut sv = ScrollView::default();
        sv.input(&Key {
            codepoint: Key::DOWN,
            ..Key::default()
        });
        assert_eq!(sv.scroll.y, 1);
        sv.input(&Key {
            codepoint: Key::PAGE_DOWN,
            ..Key::default()
        });
        assert_eq!(sv.scroll.y, 33);
        sv.input(&Key {
            codepoint: Key::HOME,
            ..Key::default()
        });
        assert_eq!(sv.scroll.y, 0);
        sv.input(&Key {
            codepoint: Key::END,
            ..Key::default()
        });
        assert_eq!(sv.scroll.y, usize::MAX);
    }

    #[test]
    fn draw_clamps_scroll_to_content() {
        let screen = screen();
        let mut sv = ScrollView::default();
        sv.scroll = Scroll { x: 1000, y: 1000 };
        // Content is 20 rows tall; the window shows 10, so max scroll_y is 10.
        // The scrollbar reserves one column, so content_cols is 4 + 1 and max
        // scroll_x is 0 against a 10-wide window.
        sv.draw(win(&screen), ContentSize { cols: 4, rows: 20 });
        assert_eq!(sv.scroll, Scroll { x: 0, y: 10 });
    }

    #[test]
    fn write_and_read_cell_translate() {
        let screen = screen();
        let mut sv = ScrollView::default();
        sv.scroll = Scroll { x: 0, y: 5 };
        sv.draw(win(&screen), ContentSize { cols: 4, rows: 20 });

        let cell = Cell {
            char: Character::new("Z", 1),
            ..Cell::default()
        };
        // Content row 5 maps to window row 0 once scrolled.
        sv.write_cell(win(&screen), 0, 5, cell.clone());
        assert_eq!(screen.borrow().read_cell(0, 0), Some(cell.clone()));
        assert_eq!(sv.read_cell(win(&screen), 0, 5), Some(cell));
    }

    #[test]
    fn out_of_bounds_is_culled() {
        let screen = screen();
        let mut sv = ScrollView::default();
        sv.scroll = Scroll { x: 0, y: 5 };
        sv.draw(win(&screen), ContentSize { cols: 4, rows: 20 });

        // A row above the visible bounds is culled on read and write.
        assert!(sv.read_cell(win(&screen), 0, 0).is_none());
        sv.write_cell(
            win(&screen),
            0,
            0,
            Cell {
                char: Character::new("Q", 1),
                ..Cell::default()
            },
        );
        let s = screen.borrow();
        let placed_q = (0..s.height).any(|r| {
            (0..s.width).any(|c| {
                s.read_cell(c, r)
                    .is_some_and(|cell| cell.char.grapheme() == "Q")
            })
        });
        assert!(!placed_q, "culled write must not place any cell");
    }
}
