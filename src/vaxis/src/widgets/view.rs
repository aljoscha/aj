//! An oversized off-screen surface that can be written in full and rendered in
//! pieces onto a [`Window`].
//!
//! A [`View`] owns its own [`Screen`], larger than the visible window, so an
//! application can lay out content once and then blit a scrolled sub-rectangle
//! onto the terminal each frame. It hands out a [`Window`] over its screen via
//! [`View::window`], the same borrow shape ordinary windows use, so the print
//! and cell APIs work unchanged.

use std::cell::RefCell;

use crate::cell::{Cell, Segment};
use crate::gwidth;
use crate::screen::Screen;
use crate::window::{PrintOptions, PrintResult, Window};

/// Configuration for [`View::new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    pub width: u16,
    pub height: u16,
}

/// An oversized off-screen surface.
///
/// The screen sits behind a [`RefCell`] so [`View::window`] can hand out a
/// [`Window`] borrowing it, exactly as [`Window`] expects.
pub struct View {
    screen: RefCell<Screen>,
}

/// Where in the target window [`View::draw`] copies the view's cells.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrawOptions {
    /// Column within the view to start copying from.
    pub x_off: u16,
    /// Row within the view to start copying from.
    pub y_off: u16,
}

/// A [`RenderConfig`] extent: fit the window, or cap at a fixed size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Extent {
    Fit,
    Max(u16),
}

impl Default for Extent {
    fn default() -> Extent {
        Extent::Fit
    }
}

/// Placement config for [`View::to_win`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderConfig {
    pub x: u16,
    pub y: u16,
    pub width: Extent,
    pub height: Extent,
}

impl View {
    /// Allocates a view sized `config.width` by `config.height`.
    pub fn new(config: Config) -> View {
        View {
            screen: RefCell::new(Screen::new(crate::Winsize {
                cols: config.width,
                rows: config.height,
                x_pixel: 0,
                y_pixel: 0,
            })),
        }
    }

    /// A [`Window`] covering the whole view.
    pub fn window(&self) -> Window<'_> {
        let (width, height) = {
            let s = self.screen.borrow();
            (s.width, s.height)
        };
        Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width,
            height,
            screen: &self.screen,
        }
    }

    /// Copies a `(win.width, win.height)`-sized sub-rectangle of the view,
    /// starting at `(opts.x_off, opts.y_off)`, into `win`.
    ///
    /// NOTE: This is the faithful equivalent of upstream's row-by-row
    /// `@memcpy`. Because a [`Cell`] owns its grapheme inline it is not `Copy`,
    /// so each row is cloned with `clone_from_slice` rather than a raw memory
    /// copy.
    ///
    /// `win` must target a different [`Screen`] than the view's own, otherwise
    /// the source and destination borrows alias and the `RefCell` panics. That
    /// mirrors upstream, where an overlapping `@memcpy` would be undefined.
    pub fn draw(&self, win: Window<'_>, opts: DrawOptions) {
        let src = self.screen.borrow();
        if opts.x_off >= src.width || opts.y_off >= src.height {
            return;
        }
        let width = win.width.min(src.width - opts.x_off);
        let height = win.height.min(src.height - opts.y_off);

        let src_width = usize::from(src.width);
        let src_len = src.buf.len();
        let w = usize::from(width);

        let mut dst = win.screen.borrow_mut();
        let dst_width = usize::from(dst.width);
        let dst_len = dst.buf.len();

        for row in 0..usize::from(height) {
            let src_row = row + usize::from(opts.y_off);
            let src_start = usize::from(opts.x_off) + src_row * src_width;
            let src_end = src_start + w;

            // The destination origin can be negative (a window clipped off the
            // top or left), so keep the math signed and drop rows/cols that
            // land outside the target buffer.
            let dst_row = i64::from(win.y_off) + i64::try_from(row).expect("row fits i64");
            let dst_col = i64::from(win.x_off);
            if dst_row < 0 || dst_col < 0 {
                continue;
            }
            let dst_start = usize::try_from(dst_col).expect("non-negative")
                + usize::try_from(dst_row).expect("non-negative") * dst_width;
            let dst_end = dst_start + w;

            if src_end > src_len || dst_end > dst_len {
                continue;
            }
            dst.buf[dst_start..dst_end].clone_from_slice(&src.buf[src_start..src_end]);
        }
    }

    /// Renders a portion of the view onto `win`, returning the bounded
    /// `(x, y)` origin actually drawn from.
    ///
    /// The requested origin and extents are clamped so the copied rectangle
    /// stays inside both the view and the window.
    pub fn to_win(&self, win: Window<'_>, config: RenderConfig) -> (u16, u16) {
        let (screen_width, screen_height) = {
            let s = self.screen.borrow();
            (s.width, s.height)
        };

        let mut x = screen_width.saturating_sub(1).min(config.x);
        let mut y = screen_height.saturating_sub(1).min(config.y);

        let width = {
            let requested = match config.width {
                Extent::Fit => win.width,
                Extent::Max(w) => win.width.min(w),
            };
            let bounded = requested.min(screen_width);
            bounded.min(
                screen_width
                    .saturating_sub(1)
                    .saturating_sub(x)
                    .saturating_add(win.width),
            )
        };
        let height = {
            let requested = match config.height {
                Extent::Fit => win.height,
                Extent::Max(h) => win.height.min(h),
            };
            let bounded = requested.min(screen_height);
            bounded.min(
                screen_height
                    .saturating_sub(1)
                    .saturating_sub(y)
                    .saturating_add(win.height),
            )
        };

        x = x.min(screen_width.saturating_sub(width));
        y = y.min(screen_height.saturating_sub(height));

        let child = win.child(crate::window::ChildOptions {
            width: Some(width),
            height: Some(height),
            ..crate::window::ChildOptions::default()
        });
        self.draw(child, DrawOptions { x_off: x, y_off: y });
        (x, y)
    }

    /// Writes `cell` to the view at `(col, row)`.
    pub fn write_cell(&self, col: u16, row: u16, cell: Cell) {
        self.screen.borrow_mut().write_cell(col, row, cell);
    }

    /// Reads the cell at `(col, row)`, or `None` when out of bounds.
    pub fn read_cell(&self, col: u16, row: u16) -> Option<Cell> {
        self.screen.borrow().read_cell(col, row)
    }

    /// Fills the whole view with the default cell.
    pub fn clear(&self) {
        self.fill(Cell {
            default: true,
            ..Cell::default()
        });
    }

    /// Display width of `s` measured with the view's width method.
    pub fn gwidth(&self, s: &str) -> u16 {
        gwidth::gwidth(s, self.screen.borrow().width_method)
    }

    /// Fills the whole view with `cell`.
    pub fn fill(&self, cell: Cell) {
        self.screen.borrow_mut().buf.fill(cell);
    }

    /// Prints `segments` to the view. See [`Window::print`].
    pub fn print(&self, segments: &[Segment], opts: PrintOptions) -> PrintResult {
        self.window().print(segments, opts)
    }

    /// Prints a single segment. Shortcut for [`View::print`] with one segment.
    pub fn print_segment(&self, segment: Segment, opts: PrintOptions) -> PrintResult {
        self.print(&[segment], opts)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::cell::Character;
    use crate::screen::Screen;

    fn target(cols: u16, rows: u16) -> RefCell<Screen> {
        RefCell::new(Screen::new(crate::Winsize {
            rows,
            cols,
            x_pixel: 0,
            y_pixel: 0,
        }))
    }

    fn win(screen: &RefCell<Screen>, w: u16, h: u16) -> Window<'_> {
        Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: w,
            height: h,
            screen,
        }
    }

    fn letter(c: &str) -> Cell {
        Cell {
            char: Character::new(c, 1),
            ..Cell::default()
        }
    }

    fn grapheme(screen: &RefCell<Screen>, c: u16, r: u16) -> String {
        screen
            .borrow()
            .read_cell(c, r)
            .unwrap()
            .char
            .grapheme()
            .to_string()
    }

    #[test]
    fn draw_copies_scrolled_sub_rect() {
        let view = View::new(Config {
            width: 20,
            height: 20,
        });
        view.write_cell(2, 3, letter("A"));
        view.write_cell(3, 3, letter("B"));
        view.write_cell(2, 4, letter("C"));

        let screen = target(5, 5);
        view.draw(win(&screen, 5, 5), DrawOptions { x_off: 2, y_off: 3 });

        assert_eq!(grapheme(&screen, 0, 0), "A");
        assert_eq!(grapheme(&screen, 1, 0), "B");
        assert_eq!(grapheme(&screen, 0, 1), "C");
    }

    #[test]
    fn print_writes_into_the_view() {
        let view = View::new(Config {
            width: 10,
            height: 4,
        });
        view.print(
            &[Segment {
                text: "hi".to_string(),
                ..Segment::default()
            }],
            PrintOptions::default(),
        );
        assert_eq!(view.read_cell(0, 0).unwrap().char.grapheme(), "h");
        assert_eq!(view.read_cell(1, 0).unwrap().char.grapheme(), "i");
    }

    #[test]
    fn to_win_clamps_origin_and_draws() {
        let view = View::new(Config {
            width: 20,
            height: 20,
        });
        view.write_cell(5, 6, letter("X"));

        let screen = target(4, 4);
        let (x, y) = view.to_win(
            win(&screen, 4, 4),
            RenderConfig {
                x: 5,
                y: 6,
                ..RenderConfig::default()
            },
        );
        assert_eq!((x, y), (5, 6));
        assert_eq!(grapheme(&screen, 0, 0), "X");
    }
}
