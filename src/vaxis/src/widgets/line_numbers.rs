//! Right-aligned line numbers drawn down the right edge of a window.
//!
//! [`LineNumbers`] draws one number per visible row, accounting for a vertical
//! scroll offset, and optionally highlights one line by filling the rest of its
//! row with the highlight style.

use crate::cell::{Cell, Character, Color, Style};
use crate::window::Window;

const DIGITS: &str = "0123456789";

/// A right-aligned line-number gutter.
pub struct LineNumbers {
    /// One past the last line number to draw. Defaults to `usize::MAX`, so the
    /// gutter is bounded only by the window height.
    pub num_lines: usize,
    /// The line to highlight, or 0 for none (line numbers are 1-based).
    pub highlighted_line: usize,
    /// Style for ordinary line numbers.
    pub style: Style,
    /// Style for the highlighted line and the fill across its row.
    pub highlighted_style: Style,
}

impl Default for LineNumbers {
    fn default() -> LineNumbers {
        LineNumbers {
            num_lines: usize::MAX,
            highlighted_line: 0,
            style: Style {
                dim: true,
                ..Style::default()
            },
            highlighted_style: Style {
                dim: true,
                bg: Color::Index(0),
                ..Style::default()
            },
        }
    }
}

impl LineNumbers {
    /// The `n`-th decimal digit of `v`, counting from the least significant.
    pub fn extract_digit(v: usize, n: usize) -> usize {
        let pow = 10usize.pow(u32::try_from(n).expect("digit index fits a u32"));
        (v / pow) % 10
    }

    /// The number of decimal digits in `v`, capped at 8.
    ///
    /// NOTE: Returns 0 for values of 100_000_000 or more, mirroring upstream's
    /// `else => 0` sentinel for numbers wider than the gutter supports.
    pub fn num_digits(v: usize) -> u8 {
        match v {
            0..=9 => 1,
            10..=99 => 2,
            100..=999 => 3,
            1_000..=9_999 => 4,
            10_000..=99_999 => 5,
            100_000..=999_999 => 6,
            1_000_000..=9_999_999 => 7,
            10_000_000..=99_999_999 => 8,
            _ => 0,
        }
    }

    /// Draws the line numbers into `win`, scrolled vertically by `y_scroll`.
    pub fn draw(&self, win: Window<'_>, y_scroll: usize) {
        for line in (1 + y_scroll)..self.num_lines {
            if line - 1 >= y_scroll.saturating_add(usize::from(win.height)) {
                break;
            }
            let highlighted = line == self.highlighted_line;
            let style = if highlighted {
                self.highlighted_style
            } else {
                self.style
            };
            let row = line.saturating_sub(y_scroll.saturating_add(1));
            let Ok(row) = u16::try_from(row) else {
                continue;
            };
            let num_digits = usize::from(Self::num_digits(line));
            for i in 0..num_digits {
                let digit = Self::extract_digit(line, i);
                let offset = u16::try_from(i + 2).unwrap_or(u16::MAX);
                win.write_cell(
                    win.width.saturating_sub(offset),
                    row,
                    Cell {
                        char: Character::new(&DIGITS[digit..digit + 1], 1),
                        style,
                        ..Cell::default()
                    },
                );
            }
            if highlighted {
                for i in (num_digits + 1)..usize::from(win.width) {
                    let Ok(col) = u16::try_from(i) else {
                        continue;
                    };
                    win.write_cell(
                        col,
                        row,
                        Cell {
                            style,
                            ..Cell::default()
                        },
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::screen::Screen;

    #[test]
    fn extract_digit_reads_place_values() {
        assert_eq!(LineNumbers::extract_digit(123, 0), 3);
        assert_eq!(LineNumbers::extract_digit(123, 1), 2);
        assert_eq!(LineNumbers::extract_digit(123, 2), 1);
    }

    #[test]
    fn num_digits_caps_and_sentinels() {
        assert_eq!(LineNumbers::num_digits(0), 1);
        assert_eq!(LineNumbers::num_digits(9), 1);
        assert_eq!(LineNumbers::num_digits(10), 2);
        assert_eq!(LineNumbers::num_digits(99_999_999), 8);
        assert_eq!(LineNumbers::num_digits(100_000_000), 0);
    }

    #[test]
    fn draw_renders_right_aligned_numbers() {
        let screen = RefCell::new(Screen::new(crate::Winsize {
            rows: 3,
            cols: 5,
            x_pixel: 0,
            y_pixel: 0,
        }));
        let win = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 5,
            height: 3,
            screen: &screen,
        };
        LineNumbers::default().draw(win, 0);
        let grapheme = |c: u16, r: u16| {
            screen
                .borrow()
                .read_cell(c, r)
                .unwrap()
                .char
                .grapheme()
                .to_string()
        };
        // Each line's last digit sits in the rightmost-but-one column (index 3).
        assert_eq!(grapheme(3, 0), "1");
        assert_eq!(grapheme(3, 1), "2");
        assert_eq!(grapheme(3, 2), "3");
    }

    #[test]
    fn draw_scrolls_and_two_digit_numbers_span_columns() {
        let screen = RefCell::new(Screen::new(crate::Winsize {
            rows: 2,
            cols: 6,
            x_pixel: 0,
            y_pixel: 0,
        }));
        let win = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 6,
            height: 2,
            screen: &screen,
        };
        // Scroll by 8 so the first visible line is 9, then 10 below it.
        LineNumbers::default().draw(win, 8);
        let grapheme = |c: u16, r: u16| {
            screen
                .borrow()
                .read_cell(c, r)
                .unwrap()
                .char
                .grapheme()
                .to_string()
        };
        assert_eq!(grapheme(4, 0), "9");
        // "10": the tens digit in column 3, the units digit in column 4.
        assert_eq!(grapheme(3, 1), "1");
        assert_eq!(grapheme(4, 1), "0");
    }
}
