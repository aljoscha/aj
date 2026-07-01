//! An immediate-mode code viewer: a [`TextView`](crate::widgets::text_view)
//! buffer drawn with a line-number gutter, indentation guides, and an optional
//! highlighted line.
//!
//! Reuses [`Buffer`] from the text view. The scroll view carries no scrollbar
//! by default, matching upstream.

use crate::cell::{Cell, Character, Color, Style};
use crate::key::Key;
use crate::widgets::line_numbers::LineNumbers;
use crate::widgets::scroll_view::{ContentSize, ScrollView};
use crate::widgets::text_view::Buffer;
use crate::window::{ChildOptions, Window};

/// Per-draw options for [`CodeView::draw`].
#[derive(Debug, Clone, Copy)]
pub struct DrawOptions {
    /// The 1-based line to highlight, or 0 for none.
    pub highlighted_line: u16,
    pub draw_line_numbers: bool,
    /// Indentation width in columns. When non-zero, a guide glyph is drawn at
    /// every multiple of this width within a line's leading whitespace.
    pub indentation: u16,
}

impl Default for DrawOptions {
    fn default() -> DrawOptions {
        DrawOptions {
            highlighted_line: 0,
            draw_line_numbers: true,
            indentation: 0,
        }
    }
}

/// An immediate-mode code viewer.
pub struct CodeView {
    pub scroll_view: ScrollView,
    /// Style applied to the highlighted line. Only its background is preserved
    /// when a per-byte style from the buffer overrides the rest.
    pub highlighted_style: Style,
    /// The cell drawn as an indentation guide.
    pub indentation_cell: Cell,
}

impl Default for CodeView {
    fn default() -> CodeView {
        CodeView {
            scroll_view: ScrollView {
                vertical_scrollbar: None,
                ..ScrollView::default()
            },
            highlighted_style: Style {
                bg: Color::Index(0),
                ..Style::default()
            },
            indentation_cell: Cell {
                char: Character::new("┆", 1),
                style: Style {
                    dim: true,
                    ..Style::default()
                },
                ..Cell::default()
            },
        }
    }
}

impl CodeView {
    /// Routes a key into the underlying scroll view.
    pub fn input(&mut self, key: &Key) {
        self.scroll_view.input(key);
    }

    /// Draws `buffer` into `win` with the gutter, guides, and highlight
    /// selected by `opts`.
    pub fn draw(&mut self, win: Window<'_>, buffer: &Buffer, opts: DrawOptions) {
        let pad_left: u16 = if opts.draw_line_numbers {
            u16::from(LineNumbers::num_digits(buffer.rows)).saturating_add(1)
        } else {
            0
        };
        self.scroll_view.draw(
            win,
            ContentSize {
                cols: buffer.cols + usize::from(pad_left),
                rows: buffer.rows,
            },
        );
        if opts.draw_line_numbers {
            let nl = LineNumbers {
                highlighted_line: usize::from(opts.highlighted_line),
                num_lines: buffer.rows.saturating_add(1),
                ..LineNumbers::default()
            };
            nl.draw(
                win.child(ChildOptions {
                    x_off: 0,
                    y_off: 0,
                    width: Some(pad_left),
                    height: Some(win.height),
                    ..ChildOptions::default()
                }),
                self.scroll_view.scroll.y,
            );
        }
        self.draw_code(
            win.child(ChildOptions {
                x_off: i32::from(pad_left),
                ..ChildOptions::default()
            }),
            buffer,
            opts,
        );
    }

    /// Draws the code region (to the right of the gutter) with indentation
    /// guides and line highlighting.
    fn draw_code(&self, win: Window<'_>, buffer: &Buffer, opts: DrawOptions) {
        let bounds = self.scroll_view.bounds(win);
        let graphemes = buffer.graphemes();
        let n = graphemes.len();
        let mut x: usize = 0;
        let mut y: usize = 0;
        let mut byte_index: usize = 0;
        let mut is_indentation = true;

        for (index, &g) in graphemes.iter().enumerate() {
            if bounds.above(y) {
                break;
            }
            let cluster = buffer.cluster(g);
            let cluster_len = cluster.len();

            if cluster == "\n" {
                if index == n - 1 {
                    break;
                }
                y += 1;
                x = 0;
                is_indentation = true;
            } else if bounds.below(y) {
                // Row scrolled above the viewport: skip but keep counting bytes.
            } else {
                let highlighted_line = y.saturating_add(1) == usize::from(opts.highlighted_line);
                let mut style = if highlighted_line {
                    self.highlighted_style
                } else {
                    Style::default()
                };
                // A per-byte style overrides everything except the highlight
                // background, so a highlighted row keeps its backdrop.
                if let Some(from_list) = buffer.style_at(byte_index) {
                    let bg = style.bg;
                    style = from_list;
                    style.bg = bg;
                }

                let width = usize::from(win.gwidth(cluster));
                if bounds.col_inside(x) {
                    if opts.indentation > 0 && cluster != " " {
                        is_indentation = false;
                    }
                    if is_indentation
                        && opts.indentation > 0
                        && x % usize::from(opts.indentation) == 0
                    {
                        let mut cell = self.indentation_cell.clone();
                        cell.style.bg = style.bg;
                        self.scroll_view.write_cell(win, x, y, cell);
                    } else {
                        self.scroll_view.write_cell(
                            win,
                            x,
                            y,
                            Cell {
                                char: Character::new(
                                    cluster,
                                    u8::try_from(width).unwrap_or(u8::MAX),
                                ),
                                style,
                                ..Cell::default()
                            },
                        );
                    }
                    if highlighted_line {
                        for hx in x.saturating_add(width)..bounds.x2 {
                            self.scroll_view.write_cell(
                                win,
                                hx,
                                y,
                                Cell {
                                    style,
                                    ..Cell::default()
                                },
                            );
                        }
                    }
                }
                x = x.saturating_add(width);
            }
            byte_index += cluster_len;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::screen::Screen;

    fn screen(cols: u16, rows: u16) -> RefCell<Screen> {
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
    fn draw_gutter_indentation_and_highlight() {
        let screen = screen(12, 4);
        let mut buf = Buffer::default();
        // Two leading spaces then "a" on line 1, then "b" and "c" below. The
        // gutter numbers lines in [1, rows+1), so three rows are needed to see
        // two numbers.
        buf.update("  a\nb\nc");
        let mut cv = CodeView::default();
        cv.draw(
            win(&screen, 12, 4),
            &buf,
            DrawOptions {
                highlighted_line: 1,
                draw_line_numbers: true,
                indentation: 2,
            },
        );

        // Gutter: line numbers 1 and 2, right-aligned in the 2-wide gutter.
        assert_eq!(grapheme(&screen, 0, 0), "1");
        assert_eq!(grapheme(&screen, 0, 1), "2");

        // Code starts at column pad_left = num_digits(1) + 1 = 2.
        // Column 0 of the code region sits on an indentation boundary and is
        // still leading whitespace, so it renders the guide glyph.
        assert_eq!(grapheme(&screen, 2, 0), "┆");
        // The first non-space content of line 1.
        assert_eq!(grapheme(&screen, 4, 0), "a");
        // The highlighted line carries the highlight background across the row,
        // including the fill past the last content cell.
        assert_eq!(
            screen.borrow().read_cell(4, 0).unwrap().style.bg,
            Color::Index(0)
        );
        assert_eq!(
            screen.borrow().read_cell(5, 0).unwrap().style.bg,
            Color::Index(0)
        );

        // Line 2 is not highlighted and its content sits at code column 0.
        assert_eq!(grapheme(&screen, 2, 1), "b");
        assert_eq!(
            screen.borrow().read_cell(2, 1).unwrap().style.bg,
            Color::Default
        );
    }
}
