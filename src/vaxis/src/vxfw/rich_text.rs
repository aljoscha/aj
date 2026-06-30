//! [`RichText`]: lays out a sequence of styled spans under width constraints,
//! with hard line breaks, optional soft wrapping, alignment, and overflow.
//!
//! NOTE(D8): This carries its own cell-based wrap engine, parallel to the
//! byte-based one in [`Text`](crate::vxfw::Text). The two are near-identical in
//! intent but operate on different units (laid-out cells here, source bytes in
//! `Text`). Unifying them would be an improvement, but it is deferred pending
//! sign-off, so both are reproduced faithfully.

use crate::cell::{Cell, Character, Segment, Style};
use crate::vxfw::{DrawContext, Overflow, Size, Surface, TextAlign, Widget, WidthBasis};

/// A styled run of text. Alias of [`Segment`], matching upstream's
/// `TextSpan = vaxis.Segment`.
pub type TextSpan = Segment;

/// A run of styled spans laid out as a leaf widget.
///
/// `draw` builds a cell buffer and never handles events, so
/// [`wants_events`](Widget::wants_events) stays the default `false`.
pub struct RichText {
    pub text: Vec<TextSpan>,
    pub text_align: TextAlign,
    pub base_style: Style,
    pub softwrap: bool,
    pub overflow: Overflow,
    pub width_basis: WidthBasis,
}

impl RichText {
    /// A left-aligned, soft-wrapping `RichText` with default base style and
    /// ellipsis overflow, sized to its longest line.
    pub fn new(text: Vec<TextSpan>) -> RichText {
        RichText {
            text,
            text_align: TextAlign::Left,
            base_style: Style::default(),
            softwrap: true,
            overflow: Overflow::Ellipsis,
            width_basis: WidthBasis::LongestLine,
        }
    }

    /// Finds the widest viewable line (under the width basis) and the number of
    /// viewable rows. Resets `iter` before returning so it can be replayed.
    fn find_container_size(&self, iter: &mut SoftwrapIterator) -> Size {
        let mut row: u16 = 0;
        let mut max_width: u16 = iter.ctx.min.width;
        if self.softwrap {
            while let Some(line) = iter.next() {
                if iter.ctx.max.outside_height(row) {
                    break;
                }
                max_width = max_width.max(line.width);
                row += 1;
            }
        } else {
            while let Some(line) = iter.next_hard_break() {
                if iter.ctx.max.outside_height(row) {
                    break;
                }
                let mut w: u16 = 0;
                for cell in &line {
                    w = w.saturating_add(u16::from(cell.char.width));
                }
                max_width = max_width.max(w);
                row += 1;
            }
        }
        let result_width = match self.width_basis {
            WidthBasis::LongestLine => match iter.ctx.max.width {
                Some(max) => max.min(max_width),
                None => max_width,
            },
            WidthBasis::Parent => iter
                .ctx
                .max
                .width
                .expect("width_basis=Parent requires a bounded max width"),
        };
        let height = row.max(iter.ctx.min.height);
        iter.reset();
        Size {
            width: result_width,
            height,
        }
    }
}

impl Widget for RichText {
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
        let mut iter = SoftwrapIterator::init(&self.text, ctx);
        let container_size = self.find_container_size(&mut iter);

        // Build at the target width and the full viewable height, then trim the
        // unused rows after drawing.
        let mut surface = Surface::with_size(container_size);
        let base = Cell {
            style: self.base_style,
            ..Cell::default()
        };
        for cell in &mut surface.buffer {
            *cell = base.clone();
        }

        let mut row: u16 = 0;
        if self.softwrap {
            while let Some(line) = iter.next() {
                if ctx.max.outside_height(row) {
                    break;
                }
                let mut col: u16 = match self.text_align {
                    TextAlign::Left => 0,
                    TextAlign::Center => (container_size.width - line.width) / 2,
                    TextAlign::Right => container_size.width - line.width,
                };
                for cell in line.cells {
                    let width = cell.char.width;
                    surface.write_cell(col, row, cell);
                    col += u16::from(width);
                }
                row += 1;
            }
        } else {
            while let Some(line) = iter.next_hard_break() {
                if ctx.max.outside_height(row) {
                    break;
                }
                let mut line_width: u16 = 0;
                for cell in &line {
                    line_width = line_width.saturating_add(u16::from(cell.char.width));
                }
                let mut col: u16 = match self.text_align {
                    TextAlign::Left => 0,
                    TextAlign::Center => container_size.width.saturating_sub(line_width) / 2,
                    TextAlign::Right => container_size.width.saturating_sub(line_width),
                };
                for cell in line {
                    if col + u16::from(cell.char.width) >= container_size.width
                        && line_width > container_size.width
                        && self.overflow == Overflow::Ellipsis
                    {
                        surface.write_cell(
                            col,
                            row,
                            Cell {
                                char: Character::new("…", 1),
                                style: cell.style,
                                ..Cell::default()
                            },
                        );
                        col = container_size.width;
                        continue;
                    }
                    let width = cell.char.width;
                    surface.write_cell(col, row, cell);
                    col += u16::from(width);
                }
                row += 1;
            }
        }

        surface.trim_height(row.max(ctx.min.height))
    }
}

/// A soft-wrapped line: the laid-out cells that fit and their total width.
struct SoftLine {
    width: u16,
    cells: Vec<Cell>,
}

/// Wraps a sequence of laid-out cells to a maximum width, breaking on spaces
/// and tabs.
///
/// Spans are expanded into a flat cell buffer once (tabs become eight spaces),
/// then the iterator walks hard breaks and packs words onto each line. A word
/// longer than the max width is broken mid-word. Trailing whitespace on a hard
/// line is trimmed before wrapping.
struct SoftwrapIterator {
    ctx: DrawContext,
    text: Vec<Cell>,
    /// The current hard line, a trimmed copy of a slice of `text`.
    line: Vec<Cell>,
    /// Position within `line`.
    index: usize,
    /// Position within `text` for the hard-break walk.
    hard_index: usize,
}

impl SoftwrapIterator {
    fn init(spans: &[TextSpan], ctx: &DrawContext) -> SoftwrapIterator {
        let mut text: Vec<Cell> = Vec::new();
        for span in spans {
            for item in ctx.grapheme_iterator(&span.text) {
                let grapheme = item.bytes(&span.text);
                if grapheme == "\t" {
                    let cell = Cell {
                        char: Character::new(" ", 1),
                        style: span.style,
                        link: span.link.clone(),
                        ..Cell::default()
                    };
                    for _ in 0..8 {
                        text.push(cell.clone());
                    }
                    continue;
                }
                let width =
                    u8::try_from(ctx.string_width(grapheme)).expect("grapheme width fits a u8");
                text.push(Cell {
                    char: Character::new(grapheme, width),
                    style: span.style,
                    link: span.link.clone(),
                    ..Cell::default()
                });
            }
        }
        SoftwrapIterator {
            ctx: *ctx,
            text,
            line: Vec::new(),
            index: 0,
            hard_index: 0,
        }
    }

    fn reset(&mut self) {
        self.index = 0;
        self.hard_index = 0;
        self.line = Vec::new();
    }

    /// Returns the next hard line as an owned cell vector, splitting on `\n` and
    /// `\r\n`.
    ///
    /// NOTE: The `\r` handling mirrors upstream exactly, including the latent
    /// bug where a lone or leading carriage return takes the "back up one"
    /// branch on the same iteration it is seen and underflows. None of the
    /// ported tests exercise `\r`, and we reproduce the behavior rather than
    /// quietly fixing it (D8).
    fn next_hard_break(&mut self) -> Option<Vec<Cell>> {
        if self.hard_index >= self.text.len() {
            return None;
        }
        let start = self.hard_index;
        let mut saw_cr = false;
        while self.hard_index < self.text.len() {
            let grapheme = self.text[self.hard_index].char.grapheme();
            let is_cr = grapheme == "\r";
            let is_lf = grapheme == "\n";
            if is_cr {
                saw_cr = true;
            }
            if is_lf {
                self.hard_index += 1;
                if saw_cr {
                    return Some(self.text[start..self.hard_index - 2].to_vec());
                }
                return Some(self.text[start..self.hard_index - 1].to_vec());
            }
            if saw_cr {
                self.hard_index -= 1;
                return Some(self.text[start..self.hard_index - 1].to_vec());
            }
            self.hard_index += 1;
        }
        Some(self.text[start..].to_vec())
    }

    /// Index just past the next word within `line`, starting from `self.index`.
    /// Skips leading whitespace, then stops at the next whitespace (or end).
    fn next_wrap(&self) -> usize {
        let mut i = self.index;
        while i < self.line.len() {
            let grapheme = self.line[i].char.grapheme();
            if grapheme == " " || grapheme == "\t" {
                i += 1;
                continue;
            }
            break;
        }
        while i < self.line.len() {
            let grapheme = self.line[i].char.grapheme();
            if grapheme == " " || grapheme == "\t" {
                return i;
            }
            i += 1;
        }
        self.line.len()
    }

    fn next(&mut self) -> Option<SoftLine> {
        // Pull the next hard line once the current one is consumed.
        if self.index == self.line.len() {
            self.line = trim_wsp_right(self.next_hard_break()?);
            self.index = 0;
        }

        let max_width = match self.ctx.max.width {
            Some(w) => w,
            None => {
                let mut width: u16 = 0;
                for cell in &self.line {
                    width += u16::from(cell.char.width);
                }
                self.index = self.line.len();
                return Some(SoftLine {
                    width,
                    cells: self.line.clone(),
                });
            }
        };

        let start = self.index;
        let mut cur_width: u16 = 0;
        while self.index < self.line.len() {
            let idx = self.next_wrap();
            // Own the word so we can advance `self.index` while reading it.
            let word: Vec<Cell> = self.line[self.index..idx].to_vec();
            let next_width: usize = word.iter().map(|c| usize::from(c.char.width)).sum();

            if usize::from(cur_width) + next_width > usize::from(max_width) {
                // Trim leading whitespace to see if the word fits a line alone.
                // The trimmed cells are all 1 wide (space/tab), so the width
                // drops by exactly the number of trimmed cells.
                let trimmed_len = trim_wsp_left_len(&word);
                let removed = word.len() - trimmed_len;
                let trimmed_width = next_width.saturating_sub(removed);
                if trimmed_width > usize::from(max_width) {
                    // Will not fit alone, so pack as many of its cells as fit.
                    for cell in &word {
                        if usize::from(cur_width) + usize::from(cell.char.width)
                            > usize::from(max_width)
                        {
                            let end = self.index;
                            return Some(SoftLine {
                                width: cur_width,
                                cells: self.line[start..end].to_vec(),
                            });
                        }
                        cur_width += u16::from(cell.char.width);
                        self.index += 1;
                    }
                }
                let end = self.index;
                // Soft-wrap: skip to the start of the next word, which is the
                // count of leading whitespace cells we trimmed.
                self.index += word.len() - trimmed_len;
                return Some(SoftLine {
                    width: cur_width,
                    cells: self.line[start..end].to_vec(),
                });
            }

            self.index = idx;
            cur_width += u16::try_from(next_width).expect("line width fits a u16");
        }
        Some(SoftLine {
            width: cur_width,
            cells: self.line[start..].to_vec(),
        })
    }
}

/// Returns `cells` with trailing space/tab cells removed.
fn trim_wsp_right(mut cells: Vec<Cell>) -> Vec<Cell> {
    let mut i = cells.len();
    while i > 0 {
        let grapheme = cells[i - 1].char.grapheme();
        if grapheme == " " || grapheme == "\t" {
            i -= 1;
            continue;
        }
        break;
    }
    cells.truncate(i);
    cells
}

/// Returns the length of `cells` after removing leading space/tab cells.
fn trim_wsp_left_len(cells: &[Cell]) -> usize {
    let mut i = 0;
    while i < cells.len() {
        let grapheme = cells[i].char.grapheme();
        if grapheme == " " || grapheme == "\t" {
            i += 1;
            continue;
        }
        break;
    }
    cells.len() - i
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Segment;
    use crate::gwidth;
    use crate::vxfw::MaxSize;

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
    fn rich_text() {
        let c = ctx(MaxSize {
            width: Some(7),
            height: Some(2),
        });

        let mut rich_text = RichText::new(vec![
            Segment {
                text: "Hello, ".into(),
                ..Segment::default()
            },
            Segment {
                text: "World".into(),
                style: Style {
                    bold: true,
                    ..Style::default()
                },
                ..Segment::default()
            },
        ]);

        // RichText soft-wraps by default.
        let surface = rich_text.draw(&c);
        assert_eq!(
            surface.size,
            Size {
                width: 6,
                height: 2
            }
        );

        // With soft wrapping off and ellipsis overflow, the line is clipped to
        // the width and the last cell becomes an ellipsis.
        rich_text.softwrap = false;
        rich_text.overflow = Overflow::Ellipsis;
        let surface = rich_text.draw(&c);
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

    #[test]
    fn long_word_wrapping() {
        let text = "a".repeat(72);
        let len = u16::try_from(text.len()).expect("length fits a u16");
        let width: u16 = 8;

        let mut rich_text = RichText::new(vec![Segment {
            text,
            ..Segment::default()
        }]);

        let c = ctx(MaxSize {
            width: Some(width),
            height: None,
        });

        let surface = rich_text.draw(&c);
        // A word longer than the width is broken every `width` cells.
        assert_eq!(surface.size.height, len / width);
    }
}
