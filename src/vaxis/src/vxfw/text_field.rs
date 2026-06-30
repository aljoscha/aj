//! [`TextField`]: a single-line, focusable text input with readline-style
//! editing, a gap buffer, and horizontal scrolling.
//!
//! The widget is stateful and interactive: it tracks the cursor and a scroll
//! offset, edits in place, and overrides [`wants_events`](Widget::wants_events)
//! so it takes part in dispatch. Text is stored in a [`Buffer`] gap buffer.
//!
//! # Callbacks
//!
//! NOTE: Upstream pairs `onChange`/`onSubmit` function pointers with a separate
//! `userdata` pointer. We drop that split: each callback is a
//! `Box<dyn FnMut(&mut EventContext, &str)>` that captures whatever state it
//! needs directly. `on_change` fires only when an edit actually changes the
//! text. `on_submit` fires on Enter/Ctrl-J and is handed the field's contents
//! while the field clears itself.
//!
//! # Word classification
//!
//! Word motion and the word-kill operations classify codepoints by Unicode
//! General Category, not by UAX#29 word segmentation. A codepoint is a word
//! constituent when it is a letter (Lu, Ll, Lt, Lm, Lo), a number (Nd, Nl, No),
//! a mark (Mn, Mc, Me), connector punctuation (Pc), or `_`. Everything else,
//! including dashes, dots, and path separators, is a boundary. This matches
//! readline's notion of a word.

use crate::cell::{Cell, Character, CursorShape, Style};
use crate::key::{Key, Modifiers};
use crate::unicode::grapheme_iterator;
use crate::vxfw::{CursorState, DrawContext, Event, EventContext, Size, Surface, Widget};

use vaxis_ucd::GeneralCategory;

/// A single-line text input.
///
/// Most fields mirror upstream's render bookkeeping and are public for parity.
/// The text itself lives in the private [`Buffer`]. The cursor is set during
/// [`draw`](Widget::draw) but the framework renders it only while this widget
/// is focused.
pub struct TextField {
    buf: Buffer,
    /// Style the text is drawn with.
    pub style: Style,
    /// Number of leading graphemes skipped when drawing (horizontal scroll).
    pub draw_offset: u16,
    /// Cursor column from the last draw.
    pub prev_cursor_col: u16,
    /// Cursor grapheme index from the last draw.
    pub prev_cursor_idx: u16,
    /// Approximate distance from an edge before scrolling kicks in.
    pub scroll_offset: u8,
    /// Width the field was last drawn at. A change resets the scroll state.
    pub prev_width: u16,
    /// The text as of the last `on_change` check, used to suppress no-op fires.
    previous_val: String,
    /// Fires when an edit changes the text. See the module docs.
    pub on_change: Option<Box<dyn FnMut(&mut EventContext, &str)>>,
    /// Fires on Enter/Ctrl-J with the field contents. See the module docs.
    pub on_submit: Option<Box<dyn FnMut(&mut EventContext, &str)>>,
}

impl TextField {
    /// An empty text field with the default style and no callbacks.
    pub fn new() -> TextField {
        TextField {
            buf: Buffer::new(),
            style: Style::default(),
            draw_offset: 0,
            prev_cursor_col: 0,
            prev_cursor_idx: 0,
            scroll_offset: 4,
            prev_width: 0,
            previous_val: String::new(),
            on_change: None,
            on_submit: None,
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

    /// Display width from the draw offset to the cursor.
    pub fn width_to_cursor(&self, ctx: &DrawContext) -> u16 {
        let mut width: u16 = 0;
        let first_half = self.buf.first_half();
        let mut i: usize = 0;
        for grapheme in ctx.grapheme_iterator(first_half) {
            if i < usize::from(self.draw_offset) {
                i += 1;
                continue;
            }
            let g = grapheme.bytes(first_half);
            width += u16::try_from(ctx.string_width(g)).expect("display width fits a u16");
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

    /// Returns the text as an owned string and clears the field.
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

    /// Consumes the event, then fires `on_change` if the text actually changed.
    fn check_changed(&mut self, ctx: &mut EventContext) {
        ctx.consume_and_redraw();
        // Skip the dupe entirely when there is nothing to notify, matching
        // upstream's early return before it tracks `previous_val`.
        if self.on_change.is_none() {
            return;
        }
        let new = self.buf.dupe();
        let changed = new != self.previous_val;
        if changed {
            if let Some(cb) = self.on_change.as_mut() {
                cb(ctx, &new);
            }
        }
        self.previous_val = new;
    }
}

impl Default for TextField {
    fn default() -> TextField {
        TextField::new()
    }
}

impl Widget for TextField {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let max_width = ctx
            .max
            .width
            .expect("TextField requires a bounded max width");
        // A width change invalidates the cached scroll offset and cursor column.
        if max_width != self.prev_width {
            self.prev_width = max_width;
            self.draw_offset = 0;
            self.prev_cursor_col = 0;
        }

        let mut surface = Surface::with_size(Size {
            width: max_width,
            height: ctx.min.height.max(1),
        });
        let base = Cell {
            style: self.style,
            ..Cell::default()
        };
        for cell in &mut surface.buffer {
            *cell = base.clone();
        }
        let style = self.style;

        let cursor_idx = self.graphemes_before_cursor();
        if cursor_idx < self.draw_offset {
            self.draw_offset = cursor_idx;
        }
        if max_width == 0 {
            return surface;
        }
        // Scroll right until the cursor fits within the visible width.
        loop {
            let width = self.width_to_cursor(ctx);
            if width >= max_width {
                self.draw_offset = self.draw_offset.saturating_add(width - max_width + 1);
            } else {
                break;
            }
        }

        self.prev_cursor_idx = cursor_idx;
        self.prev_cursor_col = 0;

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
                let w = u8::try_from(ctx.string_width(g)).expect("grapheme width fits a u8");
                // NOTE: the first half tests `col + w >= max_width` while the
                // second half below tests `col + w > max_width`. We reproduce
                // the upstream off-by-one asymmetry exactly.
                if col + u16::from(w) >= max_width {
                    surface.write_cell(max_width - 1, 0, ellipsis_cell(style));
                    break;
                }
                surface.write_cell(
                    col,
                    0,
                    Cell {
                        char: Character::new(g, w),
                        style,
                        ..Cell::default()
                    },
                );
                col += u16::from(w);
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
                let w = u8::try_from(ctx.string_width(g)).expect("grapheme width fits a u8");
                if col + u16::from(w) > max_width {
                    surface.write_cell(max_width - 1, 0, ellipsis_cell(style));
                    break;
                }
                surface.write_cell(
                    col,
                    0,
                    Cell {
                        char: Character::new(g, w),
                        style,
                        ..Cell::default()
                    },
                );
                col += u16::from(w);
                i += 1;
                if i == cursor_idx {
                    self.prev_cursor_col = col;
                }
            }
        }

        if self.draw_offset > 0 {
            surface.write_cell(0, 0, ellipsis_cell(style));
        }
        surface.cursor = Some(CursorState {
            row: 0,
            col: self.prev_cursor_col,
            shape: CursorShape::Default,
        });
        surface
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::FocusOut | Event::FocusIn => ctx.redraw = true,
            Event::KeyPress(key) => {
                if key.matches(Key::BACKSPACE, Modifiers::empty()) {
                    self.delete_before_cursor();
                    self.check_changed(ctx);
                } else if key.matches(Key::DELETE, Modifiers::empty())
                    || key.matches(u32::from('d'), Modifiers::CTRL)
                {
                    self.delete_after_cursor();
                    self.check_changed(ctx);
                } else if key.matches(Key::LEFT, Modifiers::empty())
                    || key.matches(u32::from('b'), Modifiers::CTRL)
                {
                    self.cursor_left();
                    ctx.consume_and_redraw();
                } else if key.matches(Key::RIGHT, Modifiers::empty())
                    || key.matches(u32::from('f'), Modifiers::CTRL)
                {
                    self.cursor_right();
                    ctx.consume_and_redraw();
                } else if key.matches(u32::from('a'), Modifiers::CTRL)
                    || key.matches(Key::HOME, Modifiers::empty())
                {
                    let n = self.buf.first_half().len();
                    self.buf.move_gap_left(n);
                    ctx.consume_and_redraw();
                } else if key.matches(u32::from('e'), Modifiers::CTRL)
                    || key.matches(Key::END, Modifiers::empty())
                {
                    let n = self.buf.second_half().len();
                    self.buf.move_gap_right(n);
                    ctx.consume_and_redraw();
                } else if key.matches(u32::from('k'), Modifiers::CTRL) {
                    self.delete_to_end();
                    self.check_changed(ctx);
                } else if key.matches(u32::from('u'), Modifiers::CTRL) {
                    self.delete_to_start();
                    self.check_changed(ctx);
                } else if key.matches(u32::from('b'), Modifiers::ALT)
                    || key.matches(Key::LEFT, Modifiers::ALT)
                {
                    self.move_backward_wordwise();
                    ctx.consume_and_redraw();
                } else if key.matches(u32::from('f'), Modifiers::ALT)
                    || key.matches(Key::RIGHT, Modifiers::ALT)
                {
                    self.move_forward_wordwise();
                    ctx.consume_and_redraw();
                } else if key.matches(Key::BACKSPACE, Modifiers::ALT) {
                    self.delete_word_before();
                    self.check_changed(ctx);
                } else if key.matches(u32::from('w'), Modifiers::CTRL) {
                    self.delete_word_before_whitespace();
                    self.check_changed(ctx);
                } else if key.matches(u32::from('d'), Modifiers::ALT) {
                    self.delete_word_after();
                    self.check_changed(ctx);
                } else if key.matches(Key::ENTER, Modifiers::empty())
                    || key.matches(u32::from('j'), Modifiers::CTRL)
                {
                    if self.on_submit.is_some() {
                        // toOwnedSlice clears the field, so grab the value first.
                        let value = self.to_owned_slice();
                        if let Some(cb) = self.on_submit.as_mut() {
                            cb(ctx, &value);
                        }
                        ctx.consume_and_redraw();
                    }
                } else if let Some(text) = &key.text {
                    self.insert_slice_at_cursor(text);
                    self.check_changed(ctx);
                }
            }
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
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
struct Buffer {
    buffer: Vec<u8>,
    cursor: usize,
    gap_size: usize,
}

impl Buffer {
    fn new() -> Buffer {
        Buffer {
            buffer: Vec::new(),
            cursor: 0,
            gap_size: 0,
        }
    }

    fn first_half(&self) -> &str {
        std::str::from_utf8(&self.buffer[..self.cursor])
            .expect("the first half splits on a UTF-8 boundary")
    }

    fn second_half(&self) -> &str {
        std::str::from_utf8(&self.buffer[self.cursor + self.gap_size..])
            .expect("the second half splits on a UTF-8 boundary")
    }

    /// Reallocates with room for `n` more bytes plus a fixed 512-byte slack, so
    /// runs of insertions amortize the copy.
    fn grow(&mut self, n: usize) {
        let second_half_len = self.buffer.len() - (self.cursor + self.gap_size);
        let new_size = self.buffer.len() + n + 512;
        let mut new_memory = vec![0u8; new_size];
        new_memory[..self.cursor].copy_from_slice(&self.buffer[..self.cursor]);
        let src_start = self.cursor + self.gap_size;
        new_memory[new_size - second_half_len..].copy_from_slice(&self.buffer[src_start..]);
        self.buffer = new_memory;
        self.gap_size = new_size - second_half_len - self.cursor;
    }

    fn insert_slice_at_cursor(&mut self, slice: &[u8]) {
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
    fn move_gap_left(&mut self, n: usize) {
        let new_idx = self.cursor.saturating_sub(n);
        let len = self.cursor - new_idx;
        let dst = new_idx + self.gap_size;
        copy_forwards(&mut self.buffer, dst, new_idx, len);
        self.cursor = new_idx;
    }

    /// Moves the gap `n` bytes right, carrying the head of the second half back.
    fn move_gap_right(&mut self, n: usize) {
        let new_idx = self.cursor + n;
        let src = self.cursor + self.gap_size;
        copy_forwards(&mut self.buffer, self.cursor, src, n);
        self.cursor = new_idx;
    }

    /// Grows the gap leftward by `n`, discarding `n` bytes before the cursor.
    fn grow_gap_left(&mut self, n: usize) {
        self.gap_size += n;
        self.cursor = self.cursor.saturating_sub(n);
    }

    /// Grows the gap rightward by `n`, discarding up to `n` bytes after the
    /// cursor.
    fn grow_gap_right(&mut self, n: usize) {
        self.gap_size = (self.gap_size + n).min(self.buffer.len() - self.cursor);
    }

    fn clear_and_free(&mut self) {
        self.cursor = 0;
        self.buffer = Vec::new();
        self.gap_size = 0;
    }

    fn clear_retaining_capacity(&mut self) {
        self.cursor = 0;
        self.gap_size = self.buffer.len();
    }

    fn to_owned_slice(&mut self) -> String {
        let slice = self.dupe();
        self.clear_and_free();
        slice
    }

    /// Total length of the stored text, excluding the gap.
    #[allow(dead_code)] // Part of the gap buffer's interface; not yet called.
    fn real_length(&self) -> usize {
        self.first_half().len() + self.second_half().len()
    }

    fn dupe(&self) -> String {
        let first_half = self.first_half();
        let second_half = self.second_half();
        let mut out = String::with_capacity(first_half.len() + second_half.len());
        out.push_str(first_half);
        out.push_str(second_half);
        out
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
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn slice_to_cursor() {
        let mut input = TextField::new();
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

    #[test]
    fn text_field() {
        // Shared state the change/submit callbacks write into.
        let seen = Rc::new(RefCell::new(String::new()));

        let mut text_field = TextField::new();
        let on_change_seen = Rc::clone(&seen);
        text_field.on_change = Some(Box::new(move |ctx, s| {
            *on_change_seen.borrow_mut() = s.to_string();
            ctx.consume_and_redraw();
        }));
        let on_submit_seen = Rc::clone(&seen);
        text_field.on_submit = Some(Box::new(move |ctx, s| {
            *on_submit_seen.borrow_mut() = s.to_string();
            ctx.consume_and_redraw();
        }));

        let mut ctx = EventContext::new();

        let key = |cp: char, text: &str| {
            Event::KeyPress(Key {
                codepoint: u32::from(cp),
                text: Some(text.into()),
                ..Key::default()
            })
        };

        text_field.handle_event(&mut ctx, &key('H', "H"));
        assert_eq!(seen.borrow().as_str(), "H");
        text_field.handle_event(&mut ctx, &key('e', "e"));
        assert_eq!(seen.borrow().as_str(), "He");
        text_field.handle_event(&mut ctx, &key('l', "l"));
        assert_eq!(seen.borrow().as_str(), "Hel");
        text_field.handle_event(&mut ctx, &key('l', "l"));
        assert_eq!(seen.borrow().as_str(), "Hell");
        text_field.handle_event(&mut ctx, &key('o', "o"));
        assert_eq!(seen.borrow().as_str(), "Hello");

        // An arrow moves the cursor; the text does not change.
        text_field.handle_event(
            &mut ctx,
            &Event::KeyPress(Key {
                codepoint: Key::LEFT,
                ..Key::default()
            }),
        );
        assert_eq!(seen.borrow().as_str(), "Hello");

        text_field.handle_event(&mut ctx, &key('_', "_"));
        assert_eq!(seen.borrow().as_str(), "Hell_o");
    }

    #[test]
    fn move_backward_wordwise_stops_at_word_boundary() {
        let mut input = TextField::new();
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
        let mut input = TextField::new();
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
        let mut input = TextField::new();
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
    fn delete_word_before_with_hyphens() {
        let mut input = TextField::new();
        input.insert_slice_at_cursor("hello-world");
        input.delete_word_before();
        assert_eq!(input.buf.first_half(), "hello-");
        assert_eq!(input.buf.second_half(), "");
        input.delete_word_before();
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn delete_word_before_whitespace_deletes_to_whitespace() {
        let mut input = TextField::new();
        input.insert_slice_at_cursor("hello-world foo.bar");
        input.delete_word_before_whitespace();
        assert_eq!(input.buf.first_half(), "hello-world ");
        input.delete_word_before_whitespace();
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn delete_word_after_with_mixed_punctuation() {
        let mut input = TextField::new();
        input.insert_slice_at_cursor("foo.bar baz");
        let n = input.buf.first_half().len();
        input.buf.move_gap_left(n);
        input.delete_word_after();
        assert_eq!(input.buf.first_half(), "");
        assert_eq!(input.buf.second_half(), ".bar baz");
        input.delete_word_after();
        assert_eq!(input.buf.first_half(), "");
        assert_eq!(input.buf.second_half(), " baz");
    }

    #[test]
    fn move_forward_wordwise_with_dots() {
        let mut input = TextField::new();
        input.insert_slice_at_cursor("foo.bar.baz");
        let n = input.buf.first_half().len();
        input.buf.move_gap_left(n);
        input.move_forward_wordwise();
        assert_eq!(input.buf.first_half(), "foo");
        input.move_forward_wordwise();
        assert_eq!(input.buf.first_half(), "foo.bar");
        input.move_forward_wordwise();
        assert_eq!(input.buf.first_half(), "foo.bar.baz");
    }

    #[test]
    fn word_motion_with_underscores_treats_them_as_word_chars() {
        let mut input = TextField::new();
        input.insert_slice_at_cursor("hello_world-test");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "hello_world-");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn word_motion_with_non_ascii_text() {
        let mut input = TextField::new();
        input.insert_slice_at_cursor("café-latte");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "café-");
        assert_eq!(input.buf.second_half(), "latte");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "");

        input.move_forward_wordwise();
        assert_eq!(input.buf.first_half(), "café");
        assert_eq!(input.buf.second_half(), "-latte");
    }

    #[test]
    fn non_ascii_punctuation_acts_as_a_separator() {
        let mut input = TextField::new();
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
        let mut input = TextField::new();
        input.insert_slice_at_cursor("hello\u{3000}world");
        input.delete_word_before_whitespace();
        assert_eq!(input.buf.first_half(), "hello\u{3000}");
        assert_eq!(input.buf.second_half(), "");
    }

    #[test]
    fn delete_word_before_with_non_ascii_text() {
        let mut input = TextField::new();
        input.insert_slice_at_cursor("über-cool");
        input.delete_word_before();
        assert_eq!(input.buf.first_half(), "über-");
        input.delete_word_before();
        assert_eq!(input.buf.first_half(), "");
    }

    #[test]
    fn word_motion_with_spaces() {
        let mut input = TextField::new();
        input.insert_slice_at_cursor("hello world");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "hello ");
        input.move_backward_wordwise();
        assert_eq!(input.buf.first_half(), "");
    }

    /// NOTE: TextField.zig has no ZWJ-emoji test. We add one to pin that
    /// `insert_slice_at_cursor` inserts whole grapheme clusters: a ZWJ sequence
    /// must land as a single grapheme rather than being split at the joiner.
    #[test]
    fn insert_zwj_emoji_grapheme() {
        let mut input = TextField::new();
        // Woman astronaut: U+1F469 WOMAN, U+200D ZWJ, U+1F680 ROCKET.
        let emoji = "\u{1F469}\u{200D}\u{1F680}";
        input.insert_slice_at_cursor(emoji);
        assert_eq!(input.buf.first_half(), emoji);
        assert_eq!(input.graphemes_before_cursor(), 1);
    }
}
