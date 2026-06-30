//! A vertical scrollbar drawn down the left edge of a window.
//!
//! [`Scrollbar`] is a value the application fills in each frame and draws onto
//! a [`Window`]. It sizes and positions the bar from the list geometry
//! (`total`, `view_size`, `top`) and draws nothing when there is nothing to
//! scroll.

use crate::cell::{Cell, Character, Style};
use crate::window::Window;

/// A vertical scrollbar.
///
/// `total` is the number of items in the list, `view_size` the number of items
/// that fit on screen, and `top` the index of the first visible item.
pub struct Scrollbar {
    /// Character drawn for each cell of the bar.
    pub character: Character,
    /// Style the bar character is drawn with.
    pub style: Style,
    /// Index of the top of the visible area.
    pub top: usize,
    /// Total number of items in the list.
    pub total: usize,
    /// Number of items that fit within the view area.
    pub view_size: usize,
}

impl Default for Scrollbar {
    fn default() -> Scrollbar {
        Scrollbar {
            character: Character::new("▐", 1),
            style: Style::default(),
            top: 0,
            total: 0,
            view_size: 0,
        }
    }
}

impl Scrollbar {
    /// Draws the bar down column 0 of `win`.
    ///
    /// Draws nothing when the list is empty or when every item already fits in
    /// the view.
    pub fn draw(&self, win: Window<'_>) {
        if self.total < 1 {
            return;
        }
        // Everything fits, so there is nothing to scroll.
        if self.view_size >= self.total {
            return;
        }
        let bar_height = (self.view_size * usize::from(win.height))
            .div_ceil(self.total)
            .max(1);
        let bar_top = self.top * usize::from(win.height) / self.total;
        for i in 0..bar_height {
            // bar_top + bar_height stays within win.height for any in-range
            // `top`, so the row always fits a u16. A pathological `top` only
            // pushes the row out of bounds, where write_cell drops it.
            if let Ok(row) = u16::try_from(i + bar_top) {
                win.write_cell(
                    0,
                    row,
                    Cell {
                        char: self.character.clone(),
                        style: self.style,
                        ..Cell::default()
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::screen::Screen;
    use crate::window::Window;

    fn new_screen() -> RefCell<Screen> {
        RefCell::new(Screen::new(crate::Winsize {
            rows: 10,
            cols: 1,
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
            width: 1,
            height: 10,
            screen,
        }
    }

    /// Counts the rows where the bar glyph was drawn into column 0.
    fn drawn_rows(screen: &RefCell<Screen>) -> Vec<u16> {
        let s = screen.borrow();
        (0..s.height)
            .filter(|&r| s.read_cell(0, r).is_some_and(|c| c.char.grapheme() == "▐"))
            .collect()
    }

    #[test]
    fn empty_or_fitting_list_draws_nothing() {
        let screen = new_screen();
        Scrollbar {
            total: 0,
            view_size: 0,
            ..Scrollbar::default()
        }
        .draw(win(&screen));
        assert!(drawn_rows(&screen).is_empty());

        let screen = new_screen();
        Scrollbar {
            total: 5,
            view_size: 10,
            ..Scrollbar::default()
        }
        .draw(win(&screen));
        assert!(drawn_rows(&screen).is_empty());
    }

    #[test]
    fn bar_height_and_top_track_geometry() {
        // 100 items, 10 visible in a 10-row window: a 1-row bar. At the top it
        // sits at row 0, scrolled to item 50 it sits at row 5.
        let screen = new_screen();
        Scrollbar {
            total: 100,
            view_size: 10,
            top: 0,
            ..Scrollbar::default()
        }
        .draw(win(&screen));
        assert_eq!(drawn_rows(&screen), vec![0]);

        let screen = new_screen();
        Scrollbar {
            total: 100,
            view_size: 10,
            top: 50,
            ..Scrollbar::default()
        }
        .draw(win(&screen));
        assert_eq!(drawn_rows(&screen), vec![5]);
    }
}
