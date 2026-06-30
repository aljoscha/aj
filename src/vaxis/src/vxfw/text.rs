//! [`Text`]: a leaf widget that lays a string out under width constraints,
//! with hard line breaks, optional soft wrapping, alignment, and overflow.

use crate::cell::{Cell, Character, Style};
use crate::vxfw::{DrawContext, Size, Surface, Widget};

/// Horizontal alignment of each line within the laid-out width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
    Right,
}

/// How a line that does not fit the width is handled when soft wrapping is off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Overflow {
    /// Replace the last visible cell with an ellipsis.
    #[default]
    Ellipsis,
    /// Clip the line at the edge.
    Clip,
}

/// What the laid-out width is based on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WidthBasis {
    /// Fill the parent's max width. Requires a bounded max width.
    Parent,
    /// Shrink to the widest line, capped at the max width.
    #[default]
    LongestLine,
}

/// A run of text laid out as a leaf widget.
///
/// `draw` builds a cell buffer and never handles events, so
/// [`wants_events`](Widget::wants_events) stays the default `false`.
pub struct Text {
    pub text: String,
    pub style: Style,
    pub text_align: TextAlign,
    pub softwrap: bool,
    pub overflow: Overflow,
    pub width_basis: WidthBasis,
}

impl Text {
    /// A left-aligned, soft-wrapping `Text` with default style and ellipsis
    /// overflow, sized to its longest line.
    pub fn new(text: impl Into<String>) -> Text {
        Text {
            text: text.into(),
            style: Style::default(),
            text_align: TextAlign::Left,
            softwrap: true,
            overflow: Overflow::Ellipsis,
            width_basis: WidthBasis::LongestLine,
        }
    }

    /// Finds the laid-out size: the widest viewable line (under the width
    /// basis) and the number of viewable rows.
    fn find_container_size(&self, ctx: &DrawContext) -> Size {
        let mut row: u16 = 0;
        let mut max_width: u16 = ctx.min.width;
        if self.softwrap {
            for line in SoftwrapIterator::new(&self.text, ctx) {
                if ctx.max.outside_height(row) {
                    break;
                }
                max_width = max_width.max(line.width);
                row += 1;
            }
        } else {
            for line in LineIterator::new(&self.text) {
                if ctx.max.outside_height(row) {
                    break;
                }
                let line_width =
                    u16::try_from(ctx.string_width(line)).expect("gwidth returns a u16");
                let resolved_line_width = match ctx.max.width {
                    Some(max) => max.min(line_width),
                    None => line_width,
                };
                max_width = max_width.max(resolved_line_width);
                row += 1;
            }
        }

        let result_width = match self.width_basis {
            WidthBasis::LongestLine => match ctx.max.width {
                Some(max) => max.min(max_width),
                None => max_width,
            },
            WidthBasis::Parent => ctx
                .max
                .width
                .expect("width_basis=Parent requires a bounded max width"),
        };
        Size {
            width: result_width,
            height: row.max(ctx.min.height),
        }
    }
}

impl Widget for Text {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        if ctx.max.width == Some(0) {
            return Surface {
                size: ctx.min,
                widget: None,
                cursor: None,
                buffer: Vec::new(),
                children: Vec::new(),
            };
        }
        let container_size = self.find_container_size(ctx);

        // Build at the target width and the full viewable height, then trim the
        // unused rows after drawing.
        let mut surface = Surface::with_size(container_size);
        // The fill carries only the color-bearing parts of the style, matching
        // upstream's base cell.
        let base_style = Style {
            fg: self.style.fg,
            bg: self.style.bg,
            reverse: self.style.reverse,
            ..Style::default()
        };
        let base = Cell {
            style: base_style,
            ..Cell::default()
        };
        for cell in &mut surface.buffer {
            *cell = base.clone();
        }

        let mut row: u16 = 0;
        if self.softwrap {
            for line in SoftwrapIterator::new(&self.text, ctx) {
                if row >= container_size.height {
                    break;
                }
                let mut col: u16 = match self.text_align {
                    TextAlign::Left => 0,
                    TextAlign::Center => container_size.width.saturating_sub(line.width) / 2,
                    TextAlign::Right => container_size.width.saturating_sub(line.width),
                };
                for ch in ctx.grapheme_iterator(line.bytes) {
                    let grapheme = ch.bytes(line.bytes);
                    if grapheme == "\t" {
                        // A tab expands to eight spaces in the soft path.
                        for i in 0..8u16 {
                            surface.write_cell(
                                col + i,
                                row,
                                Cell {
                                    char: Character::new(" ", 1),
                                    style: self.style,
                                    ..Cell::default()
                                },
                            );
                        }
                        col += 8;
                        continue;
                    }
                    let grapheme_width =
                        u8::try_from(ctx.string_width(grapheme)).expect("grapheme width fits a u8");
                    surface.write_cell(
                        col,
                        row,
                        Cell {
                            char: Character::new(grapheme, grapheme_width),
                            style: self.style,
                            ..Cell::default()
                        },
                    );
                    col += u16::from(grapheme_width);
                }
                row += 1;
            }
        } else {
            for line in LineIterator::new(&self.text) {
                if row >= container_size.height {
                    break;
                }
                // A tab is measured as zero width by `string_width`, so we add
                // 7 per tab to approximate its 8-cell display width. This
                // mirrors upstream and is independent of how tabs render below.
                let line_width = ctx.string_width(line) + 7 * line.matches('\t').count();
                let resolved_line_width = usize::from(container_size.width).min(line_width);
                let resolved_line_width = u16::try_from(resolved_line_width)
                    .expect("resolved width is bounded by the container width");
                let mut col: u16 = match self.text_align {
                    TextAlign::Left => 0,
                    TextAlign::Center => {
                        container_size.width.saturating_sub(resolved_line_width) / 2
                    }
                    TextAlign::Right => container_size.width.saturating_sub(resolved_line_width),
                };
                for ch in ctx.grapheme_iterator(line) {
                    if col >= container_size.width {
                        break;
                    }
                    let grapheme = ch.bytes(line);
                    let grapheme_width =
                        u8::try_from(ctx.string_width(grapheme)).expect("grapheme width fits a u8");

                    if col + u16::from(grapheme_width) >= container_size.width
                        && line_width > usize::from(container_size.width)
                        && self.overflow == Overflow::Ellipsis
                    {
                        surface.write_cell(
                            col,
                            row,
                            Cell {
                                char: Character::new("…", 1),
                                style: self.style,
                                ..Cell::default()
                            },
                        );
                        col = container_size.width;
                    } else {
                        surface.write_cell(
                            col,
                            row,
                            Cell {
                                char: Character::new(grapheme, grapheme_width),
                                style: self.style,
                                ..Cell::default()
                            },
                        );
                        col += u16::from(grapheme_width);
                    }
                }
                row += 1;
            }
        }

        surface.trim_height(row.max(ctx.min.height))
    }
}

/// Bytes that separate words for soft wrapping: space and tab.
const SOFT_BREAKS: &[u8] = b" \t";
/// Bytes that separate hard lines.
const HARD_BREAKS: &[u8] = b"\r\n";

/// Index of the first byte at or after `start` that is in `set`.
fn index_of_any_pos(bytes: &[u8], start: usize, set: &[u8]) -> Option<usize> {
    (start..bytes.len()).find(|&i| set.contains(&bytes[i]))
}

/// Index of the first byte at or after `start` that is not in `set`.
fn index_of_none_pos(bytes: &[u8], start: usize, set: &[u8]) -> Option<usize> {
    (start..bytes.len()).find(|&i| !set.contains(&bytes[i]))
}

/// Trims trailing bytes in `set` from `s`. The break sets are all ASCII, so the
/// trim point is always a UTF-8 boundary.
fn trim_end<'a>(s: &'a str, set: &[u8]) -> &'a str {
    let bytes = s.as_bytes();
    let mut end = bytes.len();
    while end > 0 && set.contains(&bytes[end - 1]) {
        end -= 1;
    }
    &s[..end]
}

/// Splits a string into hard lines on `\r`, `\n`, or `\r\n`.
///
/// The break sequence is consumed, so a `\r\n\r\n` run yields an empty line
/// between the two breaks.
struct LineIterator<'a> {
    buf: &'a str,
    index: usize,
}

impl<'a> LineIterator<'a> {
    fn new(buf: &'a str) -> LineIterator<'a> {
        LineIterator { buf, index: 0 }
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
        match index_of_any_pos(self.buf.as_bytes(), self.index, HARD_BREAKS) {
            None => {
                self.index = self.buf.len();
                Some(&self.buf[start..])
            }
            Some(end) => {
                self.index = end;
                // CR before LF so a `\r\n` pair is consumed as one break.
                self.consume_cr();
                self.consume_lf();
                Some(&self.buf[start..end])
            }
        }
    }
}

/// A soft-wrapped line: the bytes that fit and their display width.
struct SoftLine<'a> {
    width: u16,
    bytes: &'a str,
}

/// Wraps a string to a maximum width, breaking on spaces and tabs.
///
/// Drives a [`LineIterator`] for hard breaks, then packs words onto each line.
/// A word longer than the max width is broken mid-word. Trailing whitespace on
/// a hard line is trimmed before wrapping.
///
/// NOTE: This reproduces upstream's byte-based wrap engine. The dead
/// `consumeLF`/`consumeCR` helpers there reference a non-existent field and are
/// never called, so they are not ported.
struct SoftwrapIterator<'a> {
    ctx: DrawContext,
    line: &'a str,
    index: usize,
    hard_iter: LineIterator<'a>,
}

impl<'a> SoftwrapIterator<'a> {
    fn new(buf: &'a str, ctx: &DrawContext) -> SoftwrapIterator<'a> {
        SoftwrapIterator {
            ctx: *ctx,
            line: "",
            index: 0,
            hard_iter: LineIterator::new(buf),
        }
    }

    /// Index just past the next word, starting from the current position. Skips
    /// leading break bytes, then stops at the next break byte (or end).
    fn next_wrap(&self) -> usize {
        let bytes = self.line.as_bytes();
        let start_pos = match index_of_none_pos(bytes, self.index, SOFT_BREAKS) {
            Some(p) => p,
            None => return self.line.len(),
        };
        index_of_any_pos(bytes, start_pos, SOFT_BREAKS).unwrap_or(self.line.len())
    }
}

impl<'a> Iterator for SoftwrapIterator<'a> {
    type Item = SoftLine<'a>;

    fn next(&mut self) -> Option<SoftLine<'a>> {
        // Pull the next hard line once the current one is consumed.
        if self.index == self.line.len() {
            self.line = trim_end(self.hard_iter.next()?, SOFT_BREAKS);
            self.index = 0;
        }

        let start = self.index;
        let mut cur_width: u16 = 0;
        while self.index < self.line.len() {
            let idx = self.next_wrap();
            let word = &self.line[self.index..idx];
            let next_width = self.ctx.string_width(word);

            if let Some(max) = self.ctx.max.width {
                if usize::from(cur_width) + next_width > usize::from(max) {
                    // The number of trailing break bytes equals the reduction
                    // in width, since break bytes are one cell each.
                    let trimmed = trim_end(word, SOFT_BREAKS);
                    let trimmed_bytes = word.len() - trimmed.len();
                    let trimmed_width = next_width - trimmed_bytes;
                    if trimmed_width > usize::from(max) {
                        // The word does not fit on a line by itself, so pack as
                        // many of its graphemes as fit on the current line.
                        for item in self.ctx.grapheme_iterator(word) {
                            let grapheme = item.bytes(word);
                            let w = self.ctx.string_width(grapheme);
                            if usize::from(cur_width) + w > usize::from(max) {
                                let end = self.index;
                                return Some(SoftLine {
                                    width: cur_width,
                                    bytes: &self.line[start..end],
                                });
                            }
                            cur_width += u16::try_from(w).expect("grapheme width fits a u16");
                            self.index += grapheme.len();
                        }
                    }
                    // Soft-wrap: emit the current line and skip to the next word.
                    let end = self.index;
                    self.index = index_of_none_pos(self.line.as_bytes(), self.index, SOFT_BREAKS)
                        .unwrap_or(self.line.len());
                    return Some(SoftLine {
                        width: cur_width,
                        bytes: &self.line[start..end],
                    });
                }
            }

            self.index = idx;
            cur_width += u16::try_from(next_width).expect("gwidth returns a u16");
        }
        Some(SoftLine {
            width: cur_width,
            bytes: &self.line[start..],
        })
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::gwidth;
    use crate::vxfw::{MaxSize, WidgetRef, draw_widget};

    fn ctx(max: MaxSize) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max,
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        }
    }

    #[test]
    fn softwrap_iterator_lf_breaks() {
        let c = ctx(MaxSize {
            width: Some(20),
            height: Some(10),
        });
        let mut iter = SoftwrapIterator::new("Hello, \n world", &c);

        let first = iter.next().expect("first line");
        assert_eq!(first.bytes, "Hello,");
        assert_eq!(first.width, 6);

        let second = iter.next().expect("second line");
        assert_eq!(second.bytes, " world");
        assert_eq!(second.width, 6);

        assert!(iter.next().is_none());
    }

    #[test]
    fn softwrap_iterator_soft_breaks_that_fit() {
        let c = ctx(MaxSize {
            width: Some(6),
            height: Some(10),
        });
        let mut iter = SoftwrapIterator::new("Hello, \nworld", &c);

        let first = iter.next().expect("first line");
        assert_eq!(first.bytes, "Hello,");
        assert_eq!(first.width, 6);

        let second = iter.next().expect("second line");
        assert_eq!(second.bytes, "world");
        assert_eq!(second.width, 5);

        assert!(iter.next().is_none());
    }

    #[test]
    fn softwrap_iterator_soft_breaks_longer_than_width() {
        let c = ctx(MaxSize {
            width: Some(6),
            height: Some(10),
        });
        let mut iter = SoftwrapIterator::new("very-long-word \nworld", &c);

        let first = iter.next().expect("first line");
        assert_eq!(first.bytes, "very-l");
        assert_eq!(first.width, 6);

        let second = iter.next().expect("second line");
        assert_eq!(second.bytes, "ong-wo");
        assert_eq!(second.width, 6);

        let third = iter.next().expect("third line");
        assert_eq!(third.bytes, "rd");
        assert_eq!(third.width, 2);

        let fourth = iter.next().expect("fourth line");
        assert_eq!(fourth.bytes, "world");
        assert_eq!(fourth.width, 5);

        assert!(iter.next().is_none());
    }

    #[test]
    fn softwrap_iterator_soft_breaks_with_leading_spaces() {
        let c = ctx(MaxSize {
            width: Some(6),
            height: Some(10),
        });
        let mut iter = SoftwrapIterator::new("Hello,        \n world", &c);

        let first = iter.next().expect("first line");
        assert_eq!(first.bytes, "Hello,");
        assert_eq!(first.width, 6);

        let second = iter.next().expect("second line");
        assert_eq!(second.bytes, " world");
        assert_eq!(second.width, 6);

        assert!(iter.next().is_none());
    }

    #[test]
    fn line_iterator_lf_breaks() {
        let mut iter = LineIterator::new("Hello, \n world");
        assert_eq!(iter.next(), Some("Hello, "));
        assert_eq!(iter.next(), Some(" world"));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn line_iterator_cr_breaks() {
        let mut iter = LineIterator::new("Hello, \r world");
        assert_eq!(iter.next(), Some("Hello, "));
        assert_eq!(iter.next(), Some(" world"));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn line_iterator_crlf_breaks() {
        let mut iter = LineIterator::new("Hello, \r\n world");
        assert_eq!(iter.next(), Some("Hello, "));
        assert_eq!(iter.next(), Some(" world"));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn line_iterator_crlf_breaks_with_empty_line() {
        let mut iter = LineIterator::new("Hello, \r\n\r\n world");
        assert_eq!(iter.next(), Some("Hello, "));
        assert_eq!(iter.next(), Some(""));
        assert_eq!(iter.next(), Some(" world"));
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn text() {
        let c = ctx(MaxSize {
            width: Some(7),
            height: Some(2),
        });

        // Text soft-wraps by default.
        let wrapped: WidgetRef = Rc::new(RefCell::new(Text::new("Hello, world")));
        let surface = draw_widget(&wrapped, &c);
        assert_eq!(
            surface.size,
            Size {
                width: 6,
                height: 2
            }
        );

        // With soft wrapping off and ellipsis overflow, the line is clipped to
        // the width and the last cell becomes an ellipsis.
        let clipped: WidgetRef = Rc::new(RefCell::new(Text {
            softwrap: false,
            overflow: Overflow::Ellipsis,
            ..Text::new("Hello, world")
        }));
        let surface = draw_widget(&clipped, &c);
        assert_eq!(
            surface.size,
            Size {
                width: 7,
                height: 1
            }
        );
        let last = surface.buffer.last().expect("non-empty buffer");
        assert_eq!(last.char.grapheme(), "…");
    }
}
