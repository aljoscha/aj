//! [`TextInput`]: a single-line text input drawn directly onto a [`Window`].
//!
//! Unlike the retained-mode [`crate::vxfw::TextField`], the application owns
//! this widget, feeds it events through [`update`](TextInput::update), and
//! calls [`draw`](TextInput::draw) each frame. Text lives in a [`Buffer`] gap
//! buffer and the cursor is set on the window via [`Window::show_cursor`].
//!
//! # Word classification
//!
//! Word motion and the word-kill operations classify codepoints by Unicode
//! General Category, not by UAX#29 word segmentation. A codepoint is a word
//! constituent when it is a letter (Lu, Ll, Lt, Lm, Lo), a number (Nd, Nl, No),
//! a mark (Mn, Mc, Me), connector punctuation (Pc), or `_`. Everything else,
//! including dashes, dots, and path separators, is a boundary. This matches
//! readline's notion of a word.

use crate::cell::{Cell, Character, Style};
use crate::key::{Key, Modifiers};
use crate::unicode::grapheme_iterator;
use crate::window::Window;

use vaxis_ucd::GeneralCategory;

/// The events [`TextInput`] handles.
///
/// Upstream is `union(enum) { key_press: Key }`. We keep the single-variant
/// enum so the surface can grow the way upstream's can.
pub enum Event {
    KeyPress(Key),
}

/// A single-line text input.
///
/// The text is stored in the private [`Buffer`]. The render bookkeeping fields
/// are public for parity with upstream. The cursor is set during
/// [`draw`](Self::draw) through [`Window::show_cursor`].
pub struct TextInput {
    buf: Buffer,
    /// Number of leading graphemes skipped when drawing (horizontal scroll).
    pub draw_offset: u16,
    /// Cursor column from the last draw.
    pub prev_cursor_col: u16,
    /// Cursor grapheme index from the last draw.
    pub prev_cursor_idx: u16,
    /// Approximate distance from an edge before scrolling kicks in.
    pub scroll_offset: u16,
}

impl TextInput {
    /// An empty text input with default render state.
    pub fn new() -> TextInput {
        TextInput {
            buf: Buffer::new(),
            draw_offset: 0,
            prev_cursor_col: 0,
            prev_cursor_idx: 0,
            scroll_offset: 4,
        }
    }

    /// Applies a key event: editing keys mutate the buffer, printable text is
    /// inserted at the cursor.
    pub fn update(&mut self, event: &Event) {
        match event {
            Event::KeyPress(key) => {
                if key.matches(Key::BACKSPACE, Modifiers::empty()) {
                    self.delete_before_cursor();
                } else if key.matches(Key::DELETE, Modifiers::empty())
                    || key.matches(u32::from('d'), Modifiers::CTRL)
                {
                    self.delete_after_cursor();
                } else if key.matches(Key::LEFT, Modifiers::empty())
                    || key.matches(u32::from('b'), Modifiers::CTRL)
                {
                    self.cursor_left();
                } else if key.matches(Key::RIGHT, Modifiers::empty())
                    || key.matches(u32::from('f'), Modifiers::CTRL)
                {
                    self.cursor_right();
                } else if key.matches(u32::from('a'), Modifiers::CTRL)
                    || key.matches(Key::HOME, Modifiers::empty())
                {
                    let n = self.buf.first_half().len();
                    self.buf.move_gap_left(n);
                } else if key.matches(u32::from('e'), Modifiers::CTRL)
                    || key.matches(Key::END, Modifiers::empty())
                {
                    let n = self.buf.second_half().len();
                    self.buf.move_gap_right(n);
                } else if key.matches(u32::from('k'), Modifiers::CTRL) {
                    self.delete_to_end();
                } else if key.matches(u32::from('u'), Modifiers::CTRL) {
                    self.delete_to_start();
                } else if key.matches(u32::from('b'), Modifiers::ALT)
                    || key.matches(Key::LEFT, Modifiers::ALT)
                {
                    self.move_backward_wordwise();
                } else if key.matches(u32::from('f'), Modifiers::ALT)
                    || key.matches(Key::RIGHT, Modifiers::ALT)
                {
                    self.move_forward_wordwise();
                } else if key.matches(Key::BACKSPACE, Modifiers::ALT) {
                    self.delete_word_before();
                } else if key.matches(u32::from('w'), Modifiers::CTRL) {
                    self.delete_word_before_whitespace();
                } else if key.matches(u32::from('d'), Modifiers::ALT) {
                    self.delete_word_after();
                } else if let Some(text) = &key.text {
                    self.insert_slice_at_cursor(text);
                }
            }
        }
    }

    /// Inserts `data` at the cursor, one grapheme cluster at a time.
    ///
    /// Splitting on clusters keeps multi-codepoint graphemes (a ZWJ emoji, a
    /// base plus combining mark) intact rather than letting a cursor or kill
    /// operation land in the middle of one.
    pub fn insert_slice_at_cursor(&mut self, data: &str) {
        for grapheme in grapheme_iterator(data) {
            self.buf
                .insert_slice_at_cursor(grapheme.bytes(data).as_bytes());
        }
    }

    /// Copies the text before the cursor into `buf` and returns it as a string.
    ///
    /// `buf` must hold at least [`byte_offset_to_cursor`](Self::byte_offset_to_cursor)
    /// bytes.
    pub fn slice_to_cursor<'a>(&self, buf: &'a mut [u8]) -> &'a str {
        assert!(buf.len() >= self.buf.cursor);
        let first_half = self.buf.first_half();
        buf[..self.buf.cursor].copy_from_slice(first_half.as_bytes());
        std::str::from_utf8(&buf[..self.buf.cursor])
            .expect("the first half splits on a UTF-8 boundary")
    }

    /// Display width from the draw offset to the cursor, measured with `win`'s
    /// width method.
    pub fn width_to_cursor(&self, win: Window<'_>) -> u16 {
        let mut width: u16 = 0;
        let first_half = self.buf.first_half();
        let mut i: usize = 0;
        for grapheme in grapheme_iterator(first_half) {
            if i < usize::from(self.draw_offset) {
                i += 1;
                continue;
            }
            width += win.gwidth(grapheme.bytes(first_half));
            i += 1;
        }
        width
    }

    /// Moves the cursor left by one grapheme cluster.
    pub fn cursor_left(&mut self) {
        // The last cluster in the first half is the one to the cursor's left.
        let mut len: usize = 0;
        for grapheme in grapheme_iterator(self.buf.first_half()) {
            len = grapheme.len;
        }
        self.buf.move_gap_left(len);
    }

    /// Moves the cursor right by one grapheme cluster.
    pub fn cursor_right(&mut self) {
        let len = match grapheme_iterator(self.buf.second_half()).next() {
            Some(grapheme) => grapheme.len,
            None => return,
        };
        self.buf.move_gap_right(len);
    }

    /// Number of grapheme clusters before the cursor.
    pub fn graphemes_before_cursor(&self) -> u16 {
        let mut count: u16 = 0;
        for _ in grapheme_iterator(self.buf.first_half()) {
            count += 1;
        }
        count
    }

    /// Draws the input onto `win` with the default style.
    pub fn draw(&mut self, win: Window<'_>) {
        self.draw_with_style(win, Style::default());
    }

    /// Draws the input onto `win` with `style`.
    ///
    /// Scrolls horizontally so the cursor stays visible, drawing an ellipsis at
    /// either edge that is clipped, then sets the cursor through
    /// [`Window::show_cursor`].
    pub fn draw_with_style(&mut self, win: Window<'_>, style: Style) {
        let cursor_idx = self.graphemes_before_cursor();
        if cursor_idx < self.draw_offset {
            self.draw_offset = cursor_idx;
        }
        if win.width == 0 {
            return;
        }
        // Scroll right until the cursor fits within the visible width.
        loop {
            let width = self.width_to_cursor(win);
            if width >= win.width {
                self.draw_offset = self.draw_offset.saturating_add(width - win.width + 1);
            } else {
                break;
            }
        }

        self.prev_cursor_idx = cursor_idx;
        self.prev_cursor_col = 0;

        // NOTE: the gap is assumed never to fall within a grapheme. We could
        // force this by moving the gap, but that is a cost we would rather not
        // pay, so we rely on edits always landing on cluster boundaries.
        let mut col: u16 = 0;
        let mut i: u16 = 0;
        {
            let first_half = self.buf.first_half();
            for grapheme in grapheme_iterator(first_half) {
                if i < self.draw_offset {
                    i += 1;
                    continue;
                }
                let g = grapheme.bytes(first_half);
                let w = win.gwidth(g);
                // NOTE: the first half tests `col + w >= win.width` while the
                // second half below tests `col + w > win.width`. We reproduce
                // the upstream off-by-one asymmetry exactly.
                if col + w >= win.width {
                    win.write_cell(win.width - 1, 0, ellipsis_cell(style));
                    break;
                }
                win.write_cell(
                    col,
                    0,
                    Cell {
                        char: Character::new(g, u8::try_from(w).unwrap_or(u8::MAX)),
                        style,
                        ..Cell::default()
                    },
                );
                col += w;
                i += 1;
                if i == cursor_idx {
                    self.prev_cursor_col = col;
                }
            }
        }
        {
            let second_half = self.buf.second_half();
            for grapheme in grapheme_iterator(second_half) {
                if i < self.draw_offset {
                    i += 1;
                    continue;
                }
                let g = grapheme.bytes(second_half);
                let w = win.gwidth(g);
                if col + w > win.width {
                    win.write_cell(win.width - 1, 0, ellipsis_cell(style));
                    break;
                }
                win.write_cell(
                    col,
                    0,
                    Cell {
                        char: Character::new(g, u8::try_from(w).unwrap_or(u8::MAX)),
                        style,
                        ..Cell::default()
                    },
                );
                col += w;
                i += 1;
                if i == cursor_idx {
                    self.prev_cursor_col = col;
                }
            }
        }
        if self.draw_offset > 0 {
            win.write_cell(0, 0, ellipsis_cell(style));
        }
        win.show_cursor(self.prev_cursor_col, 0);
    }

    /// Clears the text, freeing the buffer, and resets the scroll state.
    pub fn clear_and_free(&mut self) {
        self.buf.clear_and_free();
        self.reset();
    }

    /// Clears the text, keeping the buffer's capacity, and resets scroll state.
    pub fn clear_retaining_capacity(&mut self) {
        self.buf.clear_retaining_capacity();
        self.reset();
    }

    /// Returns the text as an owned string and clears the input.
    pub fn to_owned_slice(&mut self) -> String {
        let slice = self.buf.to_owned_slice();
        self.reset();
        slice
    }

    /// Resets the scroll and cursor-render bookkeeping. Leaves the text alone.
    pub fn reset(&mut self) {
        self.draw_offset = 0;
        self.prev_cursor_col = 0;
        self.prev_cursor_idx = 0;
    }

    /// Number of bytes before the cursor.
    pub fn byte_offset_to_cursor(&self) -> usize {
        self.buf.cursor
    }

    /// Deletes from the cursor to the end of the text.
    pub fn delete_to_end(&mut self) {
        let n = self.buf.second_half().len();
        self.buf.grow_gap_right(n);
    }

    /// Deletes from the start of the text to the cursor.
    pub fn delete_to_start(&mut self) {
        let n = self.buf.cursor;
        self.buf.grow_gap_left(n);
    }

    /// Deletes the grapheme cluster before the cursor.
    pub fn delete_before_cursor(&mut self) {
        let mut len: usize = 0;
        for grapheme in grapheme_iterator(self.buf.first_half()) {
            len = grapheme.len;
        }
        self.buf.grow_gap_left(len);
    }

    /// Deletes the grapheme cluster after the cursor.
    pub fn delete_after_cursor(&mut self) {
        let len = match grapheme_iterator(self.buf.second_half()).next() {
            Some(grapheme) => grapheme.len,
            None => return,
        };
        self.buf.grow_gap_right(len);
    }

    /// Moves the cursor backward by one word.
    ///
    /// Skips trailing non-word codepoints, then the word itself, matching
    /// readline's backward-word.
    pub fn move_backward_wordwise(&mut self) {
        let i = {
            let first_half = self.buf.first_half().as_bytes();
            let mut i = first_half.len();
            while i > 0 {
                let decoded = decode_codepoint_before(first_half, i);
                if is_word_codepoint(decoded.cp) {
                    break;
                }
                i = decoded.start;
            }
            while i > 0 {
                let decoded = decode_codepoint_before(first_half, i);
                if !is_word_codepoint(decoded.cp) {
                    break;
                }
                i = decoded.start;
            }
            i
        };
        self.buf.move_gap_left(self.buf.cursor - i);
    }

    /// Moves the cursor forward by one word.
    ///
    /// Skips leading non-word codepoints, then the word itself, landing at the
    /// end of the next word, matching readline's forward-word.
    pub fn move_forward_wordwise(&mut self) {
        let i = {
            let second_half = self.buf.second_half().as_bytes();
            let mut i = 0usize;
            while i < second_half.len() {
                let decoded = decode_codepoint_at(second_half, i);
                if is_word_codepoint(decoded.cp) {
                    break;
                }
                i += decoded.len;
            }
            while i < second_half.len() {
                let decoded = decode_codepoint_at(second_half, i);
                if !is_word_codepoint(decoded.cp) {
                    break;
                }
                i += decoded.len;
            }
            i
        };
        self.buf.move_gap_right(i);
    }

    /// Deletes the word before the cursor by character class (Alt+Backspace).
    pub fn delete_word_before(&mut self) {
        let pre = self.buf.cursor;
        self.move_backward_wordwise();
        self.buf.grow_gap_right(pre - self.buf.cursor);
    }

    /// Deletes the word before the cursor by whitespace boundaries (Ctrl+W).
    pub fn delete_word_before_whitespace(&mut self) {
        let i = {
            let first_half = self.buf.first_half().as_bytes();
            let mut i = first_half.len();
            while i > 0 {
                let decoded = decode_codepoint_before(first_half, i);
                if !is_whitespace_codepoint(decoded.cp) {
                    break;
                }
                i = decoded.start;
            }
            while i > 0 {
                let decoded = decode_codepoint_before(first_half, i);
                if is_whitespace_codepoint(decoded.cp) {
                    break;
                }
                i = decoded.start;
            }
            i
        };
        let to_delete = self.buf.cursor - i;
        self.buf.move_gap_left(to_delete);
        self.buf.grow_gap_right(to_delete);
    }

    /// Deletes the word after the cursor by character class (Alt+D).
    pub fn delete_word_after(&mut self) {
        let i = {
            let second_half = self.buf.second_half().as_bytes();
            let mut i = 0usize;
            while i < second_half.len() {
                let decoded = decode_codepoint_at(second_half, i);
                if is_word_codepoint(decoded.cp) {
                    break;
                }
                i += decoded.len;
            }
            while i < second_half.len() {
                let decoded = decode_codepoint_at(second_half, i);
                if !is_word_codepoint(decoded.cp) {
                    break;
                }
                i += decoded.len;
            }
            i
        };
        self.buf.grow_gap_right(i);
    }
}

impl Default for TextInput {
    fn default() -> TextInput {
        TextInput::new()
    }
}

/// An ellipsis cell styled like the field, used for the scroll markers.
fn ellipsis_cell(style: Style) -> Cell {
    Cell {
        char: Character::new("…", 1),
        style,
        ..Cell::default()
    }
}

/// A decoded codepoint plus where it sits in the byte slice it came from.
struct DecodedCodepoint {
    cp: u32,
    start: usize,
    len: usize,
}

/// Length in bytes of the UTF-8 sequence a lead byte introduces, defaulting to
/// 1 for continuation or otherwise invalid lead bytes.
fn utf8_byte_sequence_length(first: u8) -> usize {
    match first {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    }
}

fn is_utf8_continuation_byte(c: u8) -> bool {
    (c & 0b1100_0000) == 0b1000_0000
}

/// Decodes the codepoint starting at `start`. On a truncated or invalid
/// sequence it falls back to the raw lead byte as a 1-byte codepoint.
fn decode_codepoint_at(bytes: &[u8], start: usize) -> DecodedCodepoint {
    let first = bytes[start];
    let len = utf8_byte_sequence_length(first);
    let capped_len = len.min(bytes.len() - start);
    let slice = &bytes[start..start + capped_len];
    match std::str::from_utf8(slice)
        .ok()
        .and_then(|s| s.chars().next())
    {
        Some(ch) => DecodedCodepoint {
            cp: u32::from(ch),
            start,
            len: capped_len,
        },
        None => DecodedCodepoint {
            cp: u32::from(first),
            start,
            len: 1,
        },
    }
}

/// Decodes the codepoint ending just before `end` by walking back over
/// continuation bytes. On an invalid sequence it falls back to the byte at
/// `end - 1` as a 1-byte codepoint.
fn decode_codepoint_before(bytes: &[u8], end: usize) -> DecodedCodepoint {
    let mut start = end - 1;
    while start > 0 && is_utf8_continuation_byte(bytes[start]) {
        start -= 1;
    }
    let slice = &bytes[start..end];
    match std::str::from_utf8(slice)
        .ok()
        .and_then(|s| s.chars().next())
    {
        Some(ch) => DecodedCodepoint {
            cp: u32::from(ch),
            start,
            len: end - start,
        },
        None => DecodedCodepoint {
            cp: u32::from(bytes[end - 1]),
            start: end - 1,
            len: 1,
        },
    }
}

/// True if `cp` is a readline-style word constituent. See the module docs for
/// the exact General Category set.
fn is_word_codepoint(cp: u32) -> bool {
    if cp == u32::from('_') {
        return true;
    }
    matches!(
        vaxis_ucd::general_category(cp),
        GeneralCategory::UppercaseLetter
            | GeneralCategory::LowercaseLetter
            | GeneralCategory::TitlecaseLetter
            | GeneralCategory::ModifierLetter
            | GeneralCategory::OtherLetter
            | GeneralCategory::DecimalNumber
            | GeneralCategory::LetterNumber
            | GeneralCategory::OtherNumber
            | GeneralCategory::NonspacingMark
            | GeneralCategory::SpacingMark
            | GeneralCategory::EnclosingMark
            | GeneralCategory::ConnectorPunctuation
    )
}

fn is_whitespace_codepoint(cp: u32) -> bool {
    matches!(cp, 0x20 | 0x09 | 0x0A | 0x0D | 0x0B | 0x0C | 0x85)
        || matches!(
            vaxis_ucd::general_category(cp),
            GeneralCategory::SpaceSeparator
                | GeneralCategory::LineSeparator
                | GeneralCategory::ParagraphSeparator
        )
}

/// A gap buffer over UTF-8 bytes.
///
/// The byte vector is split into a first half `[0, cursor)`, a gap
/// `[cursor, cursor + gap_size)`, and a second half `[cursor + gap_size, len)`.
/// The cursor always sits on a UTF-8 boundary, so each half is independently
/// valid UTF-8. Rust owns the allocation, so there is no allocator field.
pub struct Buffer {
    buffer: Vec<u8>,
    cursor: usize,
    gap_size: usize,
}

impl Buffer {
    /// An empty gap buffer.
    pub fn new() -> Buffer {
        Buffer {
            buffer: Vec::new(),
            cursor: 0,
            gap_size: 0,
        }
    }

    /// The text before the cursor.
    pub fn first_half(&self) -> &str {
        std::str::from_utf8(&self.buffer[..self.cursor])
            .expect("the first half splits on a UTF-8 boundary")
    }

    /// The text after the cursor.
    pub fn second_half(&self) -> &str {
        std::str::from_utf8(&self.buffer[self.cursor + self.gap_size..])
            .expect("the second half splits on a UTF-8 boundary")
    }

    /// Reallocates with room for `n` more bytes plus a fixed 512-byte slack, so
    /// runs of insertions amortize the copy.
    pub fn grow(&mut self, n: usize) {
        let second_half_len = self.buffer.len() - (self.cursor + self.gap_size);
        let new_size = self.buffer.len() + n + 512;
        let mut new_memory = vec![0u8; new_size];
        new_memory[..self.cursor].copy_from_slice(&self.buffer[..self.cursor]);
        let src_start = self.cursor + self.gap_size;
        new_memory[new_size - second_half_len..].copy_from_slice(&self.buffer[src_start..]);
        self.buffer = new_memory;
        self.gap_size = new_size - second_half_len - self.cursor;
    }

    /// Inserts `slice` at the cursor, growing the gap first if it is too small.
    pub fn insert_slice_at_cursor(&mut self, slice: &[u8]) {
        if slice.is_empty() {
            return;
        }
        if self.gap_size <= slice.len() {
            self.grow(slice.len());
        }
        self.buffer[self.cursor..self.cursor + slice.len()].copy_from_slice(slice);
        self.cursor += slice.len();
        self.gap_size -= slice.len();
    }

    /// Moves the gap `n` bytes left, carrying the tail of the first half across.
    pub fn move_gap_left(&mut self, n: usize) {
        let new_idx = self.cursor.saturating_sub(n);
        let len = self.cursor - new_idx;
        let dst = new_idx + self.gap_size;
        copy_forwards(&mut self.buffer, dst, new_idx, len);
        self.cursor = new_idx;
    }

    /// Moves the gap `n` bytes right, carrying the head of the second half back.
    pub fn move_gap_right(&mut self, n: usize) {
        let new_idx = self.cursor + n;
        let src = self.cursor + self.gap_size;
        copy_forwards(&mut self.buffer, self.cursor, src, n);
        self.cursor = new_idx;
    }

    /// Grows the gap leftward by `n`, discarding `n` bytes before the cursor.
    pub fn grow_gap_left(&mut self, n: usize) {
        self.gap_size += n;
        self.cursor = self.cursor.saturating_sub(n);
    }

    /// Grows the gap rightward by `n`, discarding up to `n` bytes after the
    /// cursor.
    pub fn grow_gap_right(&mut self, n: usize) {
        self.gap_size = (self.gap_size + n).min(self.buffer.len() - self.cursor);
    }

    /// Clears the text and frees the backing storage.
    pub fn clear_and_free(&mut self) {
        self.cursor = 0;
        self.buffer = Vec::new();
        self.gap_size = 0;
    }

    /// Clears the text but keeps the backing storage as gap.
    pub fn clear_retaining_capacity(&mut self) {
        self.cursor = 0;
        self.gap_size = self.buffer.len();
    }

    /// Returns the text as an owned string and clears the buffer.
    pub fn to_owned_slice(&mut self) -> String {
        let slice = self.dupe();
        self.clear_and_free();
        slice
    }

    /// Total length of the stored text, excluding the gap.
    #[allow(dead_code)] // Part of the gap buffer's interface; not yet called.
    pub fn real_length(&self) -> usize {
        self.first_half().len() + self.second_half().len()
    }

    /// Both halves concatenated into a fresh string.
    fn dupe(&self) -> String {
        let first_half = self.first_half();
        let second_half = self.second_half();
        let mut out = String::with_capacity(first_half.len() + second_half.len());
        out.push_str(first_half);
        out.push_str(second_half);
        out
    }
}

impl Default for Buffer {
    fn default() -> Buffer {
        Buffer::new()
    }
}

/// Copies `len` bytes within `buf` from `src..` to `dst..`, front to back.
///
/// Mirrors `std.mem.copyForwards`. The gap moves never overlap in a way a
/// forward copy gets wrong: the gap dwarfs any single move, so source and
/// destination never collide.
fn copy_forwards(buf: &mut [u8], dst: usize, src: usize, len: usize) {
    for i in 0..len {
        buf[dst + i] = buf[src + i];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assertion() {
        // Woman astronaut: U+1F469 WOMAN, U+200D ZWJ, U+1F680 ROCKET. Inserting
        // the same multi-codepoint grapheme six times must not crash.
        let astronaut = "\u{1F469}\u{200D}\u{1F680}";
        let key = Key {
            text: Some(astronaut.into()),
            codepoint: u32::from('\u{1F469}'),
            ..Key::default()
        };
        let event = Event::KeyPress(key);
        let mut input = TextInput::new();
        for _ in 0..6 {
            input.update(&event);
        }
    }

    #[test]
    fn slice_to_cursor() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("hello, world");
        input.cursor_left();
        input.cursor_left();
        input.cursor_left();
        let mut scratch = [0u8; 32];
        assert_eq!(input.slice_to_cursor(&mut scratch), "hello, wo");
        input.cursor_right();
        assert_eq!(input.slice_to_cursor(&mut scratch), "hello, wor");
    }

    #[test]
    fn move_backward_wordwise_stops_at_word_boundary() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("hello-world");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "hello-");
        assert_eq!(input.buf.second_half(), "world");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "");
        assert_eq!(input.buf.second_half(), "hello-world");
    }

    #[test]
    fn move_forward_wordwise_stops_at_end_of_word() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("hello-world");
        let n = input.buf.first_half().len();
        input.buf.move_gap_left(n);
        input.move_forward_wordwise();
        // Stops at the end of "hello": "hello|-world".
        assert_eq!(input.buf.first_half(), "hello");
        assert_eq!(input.buf.second_half(), "-world");
        input.move_forward_wordwise();
        // Skips "-" then stops at the end of "world": "hello-world|".
        assert_eq!(input.buf.first_half(), "hello-world");
        assert_eq!(input.buf.second_half(), "");
    }

    #[test]
    fn move_backward_wordwise_with_path_separators() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("/usr/local/bin");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "/usr/local/");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "/usr/");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "/");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn move_forward_wordwise_with_dots() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("foo.bar.baz");
        let n = input.buf.first_half().len();
        input.buf.move_gap_left(n);
        input.move_forward_wordwise();
        // Stops at end of "foo": "foo|.bar.baz".
        assert_eq!(input.buf.first_half(), "foo");
        input.move_forward_wordwise();
        // Skips "." then stops at end of "bar": "foo.bar|.baz".
        assert_eq!(input.buf.first_half(), "foo.bar");
        input.move_forward_wordwise();
        // Skips "." then stops at end of "baz": "foo.bar.baz|".
        assert_eq!(input.buf.first_half(), "foo.bar.baz");
    }

    #[test]
    fn delete_word_before_with_hyphens() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("hello-world");
        input.delete_word_before();
        // Deletes "world" only: "hello-|".
        assert_eq!(input.buf.first_half(), "hello-");
        assert_eq!(input.buf.second_half(), "");
        input.delete_word_before();
        // Skips "-" and deletes "hello": "|".
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn delete_word_before_whitespace_deletes_to_whitespace() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("hello-world foo.bar");
        input.delete_word_before_whitespace();
        // Deletes "foo.bar" (the entire whitespace-delimited word).
        assert_eq!(input.buf.first_half(), "hello-world ");
        input.delete_word_before_whitespace();
        // Deletes " hello-world".
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn delete_word_after_with_mixed_punctuation() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("foo.bar baz");
        let n = input.buf.first_half().len();
        input.buf.move_gap_left(n);
        input.delete_word_after();
        // kill-word: skip non-word (none), skip word "foo" -> delete "foo".
        assert_eq!(input.buf.first_half(), "");
        assert_eq!(input.buf.second_half(), ".bar baz");
        input.delete_word_after();
        // kill-word: skip "." (non-word), skip "bar" (word) -> delete ".bar".
        assert_eq!(input.buf.first_half(), "");
        assert_eq!(input.buf.second_half(), " baz");
    }

    #[test]
    fn word_motion_with_underscores_treats_them_as_word_chars() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("hello_world-test");
        input.move_backward_wordwise();
        // "test" is a word, stop before it: "hello_world-|test".
        assert_eq!(input.buf.first_half(), "hello_world-");
        input.move_backward_wordwise();
        // "hello_world" is one word (underscore is a word char):
        // "|hello_world-test".
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn word_motion_with_non_ascii_text() {
        let mut input = TextInput::new();
        // "café-latte": the é is multi-byte UTF-8 and must not be split.
        input.insert_slice_at_cursor("café-latte");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "café-");
        assert_eq!(input.buf.second_half(), "latte");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "");

        input.move_forward_wordwise();
        // Stops at the end of "café".
        assert_eq!(input.buf.first_half(), "caf\u{e9}");
        assert_eq!(input.buf.second_half(), "-latte");
    }

    #[test]
    fn non_ascii_punctuation_acts_as_a_separator() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("hello\u{2014}world");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "hello\u{2014}");
        assert_eq!(input.buf.second_half(), "world");

        let n = input.buf.first_half().len();
        input.buf.move_gap_left(n);
        input.move_forward_wordwise();
        assert_eq!(input.buf.first_half(), "hello");
        assert_eq!(input.buf.second_half(), "\u{2014}world");
    }

    #[test]
    fn delete_word_before_whitespace_handles_unicode_whitespace() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("hello\u{3000}world");
        input.delete_word_before_whitespace();
        assert_eq!(input.buf.first_half(), "hello\u{3000}");
        assert_eq!(input.buf.second_half(), "");
    }

    #[test]
    fn delete_word_before_with_non_ascii_text() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("über-cool");
        input.delete_word_before();
        assert_eq!(input.buf.first_half(), "über-");
        input.delete_word_before();
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn word_motion_with_spaces() {
        let mut input = TextInput::new();
        input.insert_slice_at_cursor("hello world");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "hello ");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn buffer() {
        let mut gap_buf = Buffer::new();

        gap_buf.insert_slice_at_cursor(b"abc");
        assert_eq!(gap_buf.first_half(), "abc");
        assert_eq!(gap_buf.second_half(), "");

        gap_buf.move_gap_left(1);
        assert_eq!(gap_buf.first_half(), "ab");
        assert_eq!(gap_buf.second_half(), "c");

        gap_buf.insert_slice_at_cursor(b" ");
        assert_eq!(gap_buf.first_half(), "ab ");
        assert_eq!(gap_buf.second_half(), "c");

        gap_buf.grow_gap_left(1);
        assert_eq!(gap_buf.first_half(), "ab");
        assert_eq!(gap_buf.second_half(), "c");
        assert_eq!(gap_buf.cursor, 2);

        gap_buf.grow_gap_right(1);
        assert_eq!(gap_buf.first_half(), "ab");
        assert_eq!(gap_buf.second_half(), "");
        assert_eq!(gap_buf.cursor, 2);
    }

    /// Smoke test for the Window-backed draw path: typing then drawing into a
    /// real screen places the text and shows the cursor at the end.
    #[test]
    fn draw_places_text_and_cursor() {
        use std::cell::RefCell;

        use crate::screen::Screen;

        let screen = RefCell::new(Screen {
            width_method: crate::gwidth::Method::Unicode,
            ..Screen::new(crate::Winsize {
                rows: 1,
                cols: 10,
                x_pixel: 0,
                y_pixel: 0,
            })
        });
        let win = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 10,
            height: 1,
            screen: &screen,
        };

        let mut input = TextInput::new();
        input.insert_slice_at_cursor("abc");
        input.draw(win);

        let s = screen.borrow();
        assert_eq!(s.read_cell(0, 0).unwrap().char.grapheme(), "a");
        assert_eq!(s.read_cell(1, 0).unwrap().char.grapheme(), "b");
        assert_eq!(s.read_cell(2, 0).unwrap().char.grapheme(), "c");
        assert!(s.cursor_vis);
        assert_eq!(s.cursor.col, 3);
    }
}
