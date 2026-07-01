//! An immediate-mode scrolling text viewer over a shared [`Buffer`].
//!
//! [`Buffer`] holds the text once (as grapheme offsets into a byte vector plus
//! a sparse per-byte style map), and [`TextView`] draws a scrolled view of it
//! through a [`ScrollView`]. Several views can draw the same buffer.
//!
//! [`CodeView`](crate::widgets::code_view::CodeView) reuses this [`Buffer`].

use std::collections::HashMap;
use std::io::Write;

use crate::cell::{Cell, Character, Style};
use crate::gwidth::{Method, gwidth};
use crate::key::Key;
use crate::unicode::grapheme_iterator;
use crate::widgets::scroll_view::{ContentSize, ScrollView};
use crate::window::Window;

/// One grapheme cluster located within a [`Buffer`]'s content bytes.
///
/// This replaces upstream's `MultiArrayList` of graphemes with a plain
/// `Vec<Grapheme>`. `offset` is a byte index into [`Buffer`]'s content, `len`
/// the cluster's byte length.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grapheme {
    pub len: u16,
    pub offset: u32,
}

/// A styled span applied to a [`Buffer`] via [`Buffer::update_style`].
///
/// `begin`/`end` are byte offsets into the buffer content, half-open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StyleSpan {
    pub begin: usize,
    pub end: usize,
    pub style: Style,
}

/// The shared text store for [`TextView`] and
/// [`CodeView`](crate::widgets::code_view::CodeView).
///
/// The content bytes live in one `Vec<u8>`. Graphemes index into it by byte
/// offset. Styles are deduplicated into `style_list` and referenced by
/// `style_map`, which is keyed by the byte offset of a grapheme's first byte
/// (the same `byte_index` the draw loop tracks). `rows` counts newlines and
/// `cols` is the widest line measured with [`Method::Unicode`].
#[derive(Debug, Clone, Default)]
pub struct Buffer {
    graphemes: Vec<Grapheme>,
    content: Vec<u8>,
    style_list: Vec<Style>,
    style_map: HashMap<usize, usize>,
    pub rows: usize,
    pub cols: usize,
    /// Column count carried across [`Buffer::append`] calls so appended text
    /// continues the current line rather than restarting at column 0.
    last_cols: usize,
}

impl Buffer {
    /// Replaces the buffer's contents. All previous text and styles are lost.
    pub fn update(&mut self, content: &str) {
        self.clear();
        self.append(content);
    }

    /// Appends `content`, segmenting it into graphemes and updating row/column
    /// counts.
    ///
    /// NOTE: Segmentation runs per call, so a grapheme cluster split across two
    /// `append` calls is treated as two clusters. This matches upstream, which
    /// re-segments only the bytes handed to each call.
    pub fn append(&mut self, content: &str) {
        let mut cols = self.last_cols;
        let base = self.content.len();

        for g in grapheme_iterator(content) {
            let cluster = g.bytes(content);
            self.graphemes.push(Grapheme {
                len: u16::try_from(cluster.len()).expect("grapheme length fits u16"),
                offset: u32::try_from(base + g.start).expect("grapheme offset fits u32"),
            });
            if cluster == "\n" {
                self.cols = self.cols.max(cols);
                cols = 0;
            } else {
                cols = cols.saturating_add(usize::from(gwidth(cluster, Method::Unicode)));
            }
        }

        self.content.extend_from_slice(content.as_bytes());
        self.last_cols = cols;
        self.cols = self.cols.max(cols);
        self.rows = self
            .rows
            .saturating_add(content.bytes().filter(|&b| b == b'\n').count());
    }

    /// Clears all buffer data, text and styles alike.
    pub fn clear(&mut self) {
        *self = Buffer::default();
    }

    /// Clears the style list and style map, leaving the text intact.
    pub fn clear_style(&mut self) {
        self.style_list.clear();
        self.style_map.clear();
    }

    /// Applies `span.style` to the byte range `[span.begin, span.end)`.
    ///
    /// Identical styles are deduplicated: `style_list` grows only for a style
    /// it has not seen, and `style_map` points every byte in the span at that
    /// entry.
    pub fn update_style(&mut self, span: StyleSpan) {
        let style_index = match self.style_list.iter().position(|s| *s == span.style) {
            Some(i) => i,
            None => {
                self.style_list.push(span.style);
                self.style_list.len() - 1
            }
        };
        for i in span.begin..span.end {
            self.style_map.insert(i, style_index);
        }
    }

    /// The graphemes in order, one per cluster.
    pub fn graphemes(&self) -> &[Grapheme] {
        &self.graphemes
    }

    /// The bytes of `grapheme` as a string slice.
    ///
    /// `grapheme` must come from this buffer, otherwise the byte range is
    /// meaningless. The content is always valid UTF-8, so the conversion never
    /// fails.
    pub fn cluster(&self, grapheme: Grapheme) -> &str {
        let start = usize::try_from(grapheme.offset).expect("offset fits usize");
        let end = start + usize::from(grapheme.len);
        std::str::from_utf8(&self.content[start..end]).expect("buffer content is valid utf-8")
    }

    /// The style attached to the grapheme starting at `byte_index`, if any.
    pub fn style_at(&self, byte_index: usize) -> Option<Style> {
        self.style_map.get(&byte_index).map(|&i| self.style_list[i])
    }

    /// A [`std::io::Write`] adapter that appends written bytes into this buffer.
    ///
    /// Replaces upstream's `GenericWriter`. Each write must be valid UTF-8, as
    /// the content store and grapheme segmentation require it.
    pub fn writer(&mut self) -> BufferWriter<'_> {
        BufferWriter { buffer: self }
    }
}

/// A [`std::io::Write`] adapter appending into a [`Buffer`].
pub struct BufferWriter<'a> {
    buffer: &'a mut Buffer,
}

impl Write for BufferWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let s = std::str::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.buffer.append(s);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An immediate-mode scrolling text viewer.
pub struct TextView {
    pub scroll_view: ScrollView,
}

impl Default for TextView {
    fn default() -> TextView {
        TextView {
            scroll_view: ScrollView::default(),
        }
    }
}

impl TextView {
    /// Routes a key into the underlying scroll view.
    pub fn input(&mut self, key: &Key) {
        self.scroll_view.input(key);
    }

    /// Draws the scrolled view of `buffer` into `win`.
    pub fn draw(&mut self, win: Window<'_>, buffer: &Buffer) {
        self.scroll_view.draw(
            win,
            ContentSize {
                cols: buffer.cols,
                rows: buffer.rows,
            },
        );

        let bounds = self.scroll_view.bounds(win);
        let graphemes = buffer.graphemes();
        let n = graphemes.len();
        let mut x: usize = 0;
        let mut y: usize = 0;
        let mut byte_index: usize = 0;

        for (index, &g) in graphemes.iter().enumerate() {
            if bounds.above(y) {
                break;
            }
            let cluster = buffer.cluster(g);
            let cluster_len = cluster.len();

            if cluster == "\n" {
                // A trailing newline draws nothing, so stop before advancing.
                if index == n - 1 {
                    break;
                }
                y = y.saturating_add(1);
                x = 0;
            } else if bounds.below(y) {
                // Row scrolled above the viewport: skip but keep counting bytes.
            } else {
                let width = usize::from(win.gwidth(cluster));
                if bounds.col_inside(x) {
                    let style = buffer.style_at(byte_index).unwrap_or_default();
                    self.scroll_view.write_cell(
                        win,
                        x,
                        y,
                        Cell {
                            char: Character::new(cluster, u8::try_from(width).unwrap_or(u8::MAX)),
                            style,
                            ..Cell::default()
                        },
                    );
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
    fn update_counts_rows_cols_and_offsets() {
        let mut buf = Buffer::default();
        buf.update("ab\ncd");
        assert_eq!(buf.rows, 1);
        assert_eq!(buf.cols, 2);

        let gs = buf.graphemes();
        assert_eq!(gs.len(), 5);
        assert_eq!(gs[0].offset, 0);
        assert_eq!(gs[3].offset, 3);
        assert_eq!(buf.cluster(gs[3]), "c");
        assert_eq!(buf.cluster(gs[2]), "\n");
    }

    #[test]
    fn append_continues_current_line() {
        let mut buf = Buffer::default();
        buf.update("ab\ncd");
        // No newline, so the appended text extends the current (second) line.
        buf.append("ef");
        assert_eq!(buf.rows, 1);
        assert_eq!(buf.cols, 4);
        assert_eq!(buf.graphemes().len(), 7);
    }

    #[test]
    fn wide_grapheme_counts_two_columns() {
        let mut buf = Buffer::default();
        // A fullwidth CJK ideograph measures two columns.
        buf.update("\u{4e16}\u{754c}");
        assert_eq!(buf.rows, 0);
        assert_eq!(buf.cols, 4);
    }

    #[test]
    fn update_style_maps_span_and_dedups() {
        let mut buf = Buffer::default();
        buf.update("hello");
        let bold = Style {
            bold: true,
            ..Style::default()
        };
        buf.update_style(StyleSpan {
            begin: 0,
            end: 3,
            style: bold,
        });
        assert_eq!(buf.style_at(0), Some(bold));
        assert_eq!(buf.style_at(2), Some(bold));
        assert_eq!(buf.style_at(3), None);
        assert_eq!(buf.style_list.len(), 1);

        // A second span with the same style reuses the existing entry.
        buf.update_style(StyleSpan {
            begin: 4,
            end: 5,
            style: bold,
        });
        assert_eq!(buf.style_list.len(), 1);
        assert_eq!(buf.style_at(4), Some(bold));
    }

    #[test]
    fn writer_appends_utf8() {
        let mut buf = Buffer::default();
        write!(buf.writer(), "ab\ncd").unwrap();
        assert_eq!(buf.rows, 1);
        assert_eq!(buf.graphemes().len(), 5);
    }

    #[test]
    fn clear_resets_everything() {
        let mut buf = Buffer::default();
        buf.update("ab\ncd");
        buf.update_style(StyleSpan {
            begin: 0,
            end: 1,
            style: Style {
                bold: true,
                ..Style::default()
            },
        });
        buf.clear();
        assert_eq!(buf.rows, 0);
        assert_eq!(buf.cols, 0);
        assert!(buf.graphemes().is_empty());
        assert_eq!(buf.style_at(0), None);
    }

    #[test]
    fn draw_places_cells_and_style() {
        let screen = screen(10, 10);
        let mut buf = Buffer::default();
        buf.update("ab\ncd");
        buf.update_style(StyleSpan {
            begin: 0,
            end: 1,
            style: Style {
                bold: true,
                ..Style::default()
            },
        });
        let mut tv = TextView::default();
        tv.draw(win(&screen, 10, 10), &buf);

        assert_eq!(grapheme(&screen, 0, 0), "a");
        assert_eq!(grapheme(&screen, 1, 0), "b");
        assert_eq!(grapheme(&screen, 0, 1), "c");
        assert_eq!(grapheme(&screen, 1, 1), "d");
        assert!(screen.borrow().read_cell(0, 0).unwrap().style.bold);
        assert!(!screen.borrow().read_cell(1, 0).unwrap().style.bold);
    }
}
