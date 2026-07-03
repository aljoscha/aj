//! [`TextArea`]: a multi-line, focusable text editor with word wrapping,
//! emacs-style editing, a kill ring, undo, sticky-column vertical movement,
//! and prompt history.
//!
//! `TextArea` is the multi-line sibling of [`TextField`](crate::vxfw::TextField).
//! Where `TextField` is a single-line gap buffer, `TextArea` holds a document as
//! one `String` per logical line and a `(line, byte col)` cursor, wraps each
//! logical line to the layout width for display, and moves the cursor
//! vertically across wrapped rows while keeping a sticky preferred column.
//!
//! # Editing engine
//!
//! The reusable, widget-free primitives live in [`crate::text`]: the
//! [`KillRing`], the [`UndoStack`], and the word-motion engine. `TextArea`
//! defaults to the three-class [`EmacsWords`] classifier, where a run of
//! punctuation is its own word, and exposes
//! [`set_word_classifier`](TextArea::set_word_classifier) to swap it.
//!
//! # Rendering
//!
//! `draw` produces a bordered [`Surface`]: a top rule, a bounded window of
//! visible wrapped rows, and a bottom rule. The border color and top-bar label
//! inlay into the top rule, and `↑ N more` / `↓ N more` indicators reflect the
//! internal scroll offset. The caret is reported through
//! [`Surface::cursor`](crate::vxfw::Surface), which the framework renders only
//! while this widget is focused.
//!
//! # Height and scroll
//!
//! `TextArea` sizes to the `DrawContext` constraints. It never reads terminal
//! rows. The visible-row cap is `min(max_visible_rows.unwrap_or(available),
//! available)` where `available` is `ctx.max.height` minus the two border rows,
//! so a flex-0 host slot grows with content up to the cap and scrolls beyond.

use std::cell::RefCell;
use std::rc::Rc;

use unicode_segmentation::UnicodeSegmentation;

use crate::cell::{Cell, Character, Color, CursorShape, Style};
use crate::gwidth;
use crate::key::{Key, Modifiers};
use crate::text::{EmacsWords, KillRing, UndoStack, WordClassifier, word_left, word_right};
use crate::vxfw::{CursorState, DrawContext, Event, EventContext, Size, Surface, Widget};

/// Theme for [`TextArea`]: structured colors and styles the app builds from its
/// palette.
///
/// The B3 autocomplete popup styling lands in [`PopupStyle`]. It is reserved
/// today so callers that build a theme now do not have to change the shape
/// later.
#[derive(Debug, Clone, Default)]
pub struct EditorTheme {
    /// Color of the top and bottom border rules and their inlaid text.
    pub border_color: Color,
    /// Styling for the inline autocomplete popup. Unused in the core editor,
    /// reserved for the autocomplete phase.
    pub popup: PopupStyle,
}

/// Styling for the inline autocomplete popup.
///
/// A placeholder for the autocomplete phase. The fields are the styles the
/// popup will draw with once it lands.
#[derive(Debug, Clone, Default)]
pub struct PopupStyle {
    /// Style for an unselected suggestion row.
    pub item: Style,
    /// Style for the highlighted suggestion row.
    pub selected: Style,
}

/// A fixed editing chord, for documentation surfaces (the help screen).
///
/// `TextArea`'s chords are hard-coded, not rebindable, so [`TextArea::bindings`]
/// returns a static slice of these as the single source of truth shared by the
/// event handler and the docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordDoc {
    /// Human-readable key label, e.g. `"Ctrl-A / Home"`.
    pub keys: &'static str,
    /// What the chord does.
    pub description: &'static str,
    /// Display group the help screen buckets the chord under.
    pub group: &'static str,
}

/// Snapshot of the document and cursor for one undo step.
#[derive(Debug, Clone)]
struct EditorSnapshot {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
}

/// The last mutating action, used to coalesce undo snapshots, accumulate kills,
/// and gate yank-pop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    None,
    Kill,
    Yank,
    TypeWord,
}

/// A single visual (screen) line produced by wrapping one logical document line
/// at the current layout width.
///
/// Byte-offset based: `start_col` is the index into `lines[logical_line]` where
/// the visible span begins, and `length` is the byte length of that span.
/// Concatenating the spans of a logical line reconstructs it exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VisualLine {
    logical_line: usize,
    start_col: usize,
    length: usize,
}

/// One atomic display segment of a logical line.
///
/// For the core editor every grapheme is its own segment, so `text` is one
/// grapheme cluster. The wrapper and the vertical-move snap treat a segment as
/// indivisible, which keeps a multi-byte grapheme (a ZWJ emoji, base plus
/// combining mark) whole across wrap boundaries and stops the cursor landing in
/// the middle of one.
#[derive(Debug, Clone, Copy)]
struct AtomicSegment<'a> {
    text: &'a str,
    start_index: usize,
}

/// A multi-line text editor widget. See the module docs for the model.
pub struct TextArea {
    // -- Document --
    /// One string per logical line. Invariant: never empty. An empty document
    /// is `vec![String::new()]`.
    lines: Vec<String>,
    /// Cursor line index.
    cursor_line: usize,
    /// Cursor byte offset within `lines[cursor_line]`.
    cursor_col: usize,

    // -- Vertical-movement state --
    /// First visible visual row. Persisted across draws as the scroll hint.
    scroll_offset: usize,
    /// Sticky column (in bytes into a visual line) for vertical movement.
    preferred_visual_col: Option<usize>,
    /// Byte offset the cursor would have occupied on its source line had the
    /// last vertical move not snapped it onto an atomic segment. The next
    /// vertical move reads this so the sticky math reflects the user's intent
    /// rather than the post-snap column. Cleared by any edit or horizontal move
    /// through [`TextArea::reset_sticky_state`].
    snapped_from_cursor_col: Option<usize>,

    // -- Editing state --
    kill_ring: KillRing,
    undo_stack: UndoStack<EditorSnapshot>,
    last_action: LastAction,
    word_classifier: Box<dyn WordClassifier>,

    // -- History --
    history: Vec<String>,
    history_index: Option<usize>,

    // -- Presentation --
    theme: EditorTheme,
    padding_x: usize,
    top_bar_label: Option<String>,
    /// Explicit cap on visible rows. `None` uses the constraint-derived cap.
    max_visible_rows: Option<usize>,

    // -- Layout state cached from the last draw --
    //
    // Navigation happens in `handle_event`, where there is no `DrawContext`, so
    // draw stashes the width it laid out at and the width-measurement method
    // here for the visual-line map to reuse. Defaults keep navigation before
    // the first draw sane.
    layout_width: usize,
    width_method: gwidth::Method,
    /// Visible-row count from the last draw. Used as the page size for
    /// PageUp/PageDown.
    last_visible_rows: usize,

    // -- Submission --
    /// If false, Enter is silently consumed instead of submitting.
    submit_enabled: bool,
    /// Set on submit, polled by the host as an alternative to `on_submit`.
    submitted_text: Option<String>,
    /// Text as of the last `on_change` check, used to suppress no-op fires.
    previous_val: String,

    /// Fires during event handling when an edit changes the text.
    pub on_change: Option<Box<dyn FnMut(&mut EventContext, &str)>>,
    /// Fires on submit with the (trimmed) contents while the editor clears.
    pub on_submit: Option<Box<dyn FnMut(&mut EventContext, &str)>>,
    /// Fires when the user types `/` at an empty start-of-message position. The
    /// `/` keystroke is swallowed, not inserted, when this is set.
    pub on_palette_trigger: Option<Box<dyn FnMut(&mut EventContext)>>,
}

impl TextArea {
    /// Maximum history entries retained. Once reached, each new entry drops the
    /// oldest.
    pub const HISTORY_LIMIT: usize = 100;

    /// A new empty editor behind an `Rc<RefCell<..>>`, matching the widget
    /// convention.
    pub fn new() -> Rc<RefCell<TextArea>> {
        Rc::new(RefCell::new(Self::new_state()))
    }

    /// Builds the bare widget state. Kept separate from [`TextArea::new`] so
    /// in-crate tests can drive a plain `TextArea` without the `Rc<RefCell<..>>`
    /// wrapper.
    fn new_state() -> TextArea {
        TextArea {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            scroll_offset: 0,
            preferred_visual_col: None,
            snapped_from_cursor_col: None,
            kill_ring: KillRing::new(),
            undo_stack: UndoStack::new(),
            last_action: LastAction::None,
            word_classifier: Box::new(EmacsWords),
            history: Vec::new(),
            history_index: None,
            theme: EditorTheme::default(),
            padding_x: 0,
            top_bar_label: None,
            max_visible_rows: None,
            layout_width: 80,
            width_method: gwidth::Method::Unicode,
            last_visible_rows: 10,
            submit_enabled: true,
            submitted_text: None,
            previous_val: String::new(),
            on_change: None,
            on_submit: None,
            on_palette_trigger: None,
        }
    }

    /// The fixed editing chords, the single source of truth for both the
    /// handler and the help screen.
    pub fn bindings() -> &'static [ChordDoc] {
        &BINDINGS
    }

    // -- Content --

    /// The full text, logical lines joined by `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// The cursor as `(line, col)`, where `col` counts Unicode scalar values
    /// from the start of the line, not bytes. For pure-ASCII input `col` equals
    /// the byte offset.
    pub fn cursor(&self) -> (usize, usize) {
        let col = self.current_line()[..self.cursor_col].chars().count();
        (self.cursor_line, col)
    }

    /// Replaces all text. The cursor moves to the end.
    ///
    /// Input is normalized: `\r\n` and lone `\r` collapse to `\n`, and `\t`
    /// expands to four spaces. `on_change` does not fire here, only on
    /// interactive edits (see [`TextArea::handle_event`]).
    pub fn set_text(&mut self, text: &str) {
        self.save_undo();
        let normalized = Self::normalize_text(text);
        self.lines = if normalized.is_empty() {
            vec![String::new()]
        } else {
            normalized.split('\n').map(str::to_string).collect()
        };
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_line].len();
        self.reset_sticky_state();
        // A wholesale replacement is a hard break: no kill accumulation, yank
        // cycle, or word-repeat coalescing should carry across it.
        self.last_action = LastAction::None;
        // Exit history browsing, dropping the draft `history_up` parked at the
        // tail if we were mid-browse.
        if self.history_index.is_some() {
            self.history.pop();
            self.history_index = None;
        }
    }

    /// Inserts `text` at the cursor as one undo unit.
    ///
    /// Input is normalized like [`TextArea::set_text`]. Other control
    /// characters are stripped so a pasted `\0` never lands in the document.
    pub fn insert_at_cursor(&mut self, text: &str) {
        self.save_undo();
        self.history_index = None;
        let normalized = Self::normalize_text(text);
        for ch in normalized.chars() {
            if ch == '\n' {
                self.insert_newline_internal();
            } else if !ch.is_control() {
                self.lines[self.cursor_line].insert(self.cursor_col, ch);
                self.cursor_col += ch.len_utf8();
            }
        }
        self.reset_sticky_state();
        self.last_action = LastAction::None;
    }

    /// Clears the document and undo history, leaving one empty line.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.reset_sticky_state();
        self.undo_stack.clear();
        self.last_action = LastAction::None;
        self.history_index = None;
    }

    /// Takes the submitted text, if any, clearing it. Returns `Some` at most
    /// once per submit. An alternative to the `on_submit` callback.
    pub fn take_submitted(&mut self) -> Option<String> {
        self.submitted_text.take()
    }

    // -- Submission config --

    /// Enables or disables submit. When disabled, Enter is silently consumed.
    pub fn set_submit_enabled(&mut self, enabled: bool) {
        self.submit_enabled = enabled;
    }

    // -- History --

    /// Adds `text` to the history for up/down navigation.
    ///
    /// Whitespace-only strings and a duplicate of the most recent entry are
    /// ignored. The ring is capped at [`TextArea::HISTORY_LIMIT`]; once full,
    /// the oldest entry is dropped.
    pub fn add_to_history(&mut self, text: &str) {
        if text.trim().is_empty() {
            self.history_index = None;
            return;
        }
        if self.history.last().is_some_and(|prev| prev == text) {
            self.history_index = None;
            return;
        }
        self.history.push(text.to_string());
        if self.history.len() > Self::HISTORY_LIMIT {
            let overflow = self.history.len() - Self::HISTORY_LIMIT;
            self.history.drain(..overflow);
        }
        self.history_index = None;
    }

    /// Splices older entries in beneath whatever the ring already holds.
    ///
    /// `entries` are oldest-first. They land before any prompts already in the
    /// ring, so submissions made this session stay the most-recent ones an Up
    /// press reaches first. Safe to call mid-browse: a browse cursor (and the
    /// draft parked at the tail) index into the ring, so we shift
    /// `history_index` by the net change at the front and only ever drop from
    /// the front to keep the draft at the tail.
    pub fn seed_history(&mut self, entries: &[String]) {
        if entries.is_empty() {
            return;
        }
        let added = entries.len();
        let mut seeded = Vec::with_capacity(added + self.history.len());
        seeded.extend(entries.iter().cloned());
        seeded.append(&mut self.history);
        self.history = seeded;
        if let Some(idx) = self.history_index.as_mut() {
            *idx += added;
        }
        if self.history.len() > Self::HISTORY_LIMIT {
            let overflow = self.history.len() - Self::HISTORY_LIMIT;
            self.history.drain(..overflow);
            if let Some(idx) = self.history_index.as_mut() {
                *idx = idx.saturating_sub(overflow);
            }
        }
    }

    // -- Presentation --

    /// Replaces the theme.
    pub fn set_theme(&mut self, theme: EditorTheme) {
        self.theme = theme;
    }

    /// Sets the border color (a thinking/bash-mode tint), independent of the
    /// rest of the theme.
    pub fn set_border_color(&mut self, color: Color) {
        self.theme.border_color = color;
    }

    /// Sets horizontal padding, in columns, on each side.
    pub fn set_padding_x(&mut self, cols: usize) {
        self.padding_x = cols;
    }

    /// Inlays a short label into the top border. `None` leaves a plain rule.
    pub fn set_top_bar_label(&mut self, label: Option<String>) {
        self.top_bar_label = label;
    }

    /// Caps the visible rows revealed before scrolling. `None` uses the
    /// constraint-derived cap.
    pub fn set_max_visible_rows(&mut self, max: Option<usize>) {
        self.max_visible_rows = max;
    }

    /// Swaps the word classifier that drives word motions and word kills. The
    /// default is [`EmacsWords`].
    pub fn set_word_classifier(&mut self, classifier: Box<dyn WordClassifier>) {
        self.word_classifier = classifier;
    }

    // -- Private: text normalization and undo --

    /// Normalizes text for storage: `\r\n` and lone `\r` collapse to `\n`, and
    /// every `\t` expands to four spaces. The internal model never holds an
    /// embedded `\r` or `\t`.
    fn normalize_text(text: &str) -> String {
        text.replace("\r\n", "\n")
            .replace('\r', "\n")
            .replace('\t', "    ")
    }

    fn save_undo(&mut self) {
        self.undo_stack.push(EditorSnapshot {
            lines: self.lines.clone(),
            cursor_line: self.cursor_line,
            cursor_col: self.cursor_col,
        });
    }

    fn restore_undo(&mut self) {
        if let Some(snapshot) = self.undo_stack.pop() {
            // Exit history browsing, dropping the draft parked at the tail if we
            // were mid-browse so the ring returns to its pre-browse shape.
            if self.history_index.is_some() {
                self.history.pop();
                self.history_index = None;
            }
            self.lines = snapshot.lines;
            self.cursor_line = snapshot.cursor_line;
            self.cursor_col = snapshot.cursor_col;
            self.reset_sticky_state();
            self.last_action = LastAction::None;
        }
    }

    fn current_line(&self) -> &str {
        &self.lines[self.cursor_line]
    }

    fn is_empty_doc(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Grapheme-cluster byte boundaries for the current line, including `0` and
    /// the line length.
    fn grapheme_boundaries(&self) -> Vec<usize> {
        let line = self.current_line();
        let mut bounds = vec![0];
        for (i, _) in line.grapheme_indices(true) {
            if i > 0 {
                bounds.push(i);
            }
        }
        bounds.push(line.len());
        bounds
    }

    /// Display width of `s` in cells, using the last draw's width method.
    fn measure(&self, s: &str) -> usize {
        usize::from(gwidth::gwidth(s, self.width_method))
    }

    // -- Horizontal movement --

    /// Moves the cursor left by one grapheme, wrapping to the end of the
    /// previous line at column zero. Any horizontal move clears the sticky
    /// column.
    fn move_left(&mut self) {
        self.reset_sticky_state();
        if self.cursor_col > 0 {
            let bounds = self.grapheme_boundaries();
            for i in (0..bounds.len()).rev() {
                if bounds[i] < self.cursor_col {
                    self.cursor_col = bounds[i];
                    return;
                }
            }
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
        }
    }

    /// Moves the cursor right by one grapheme, wrapping to the start of the next
    /// line at end-of-line. Clears the sticky column.
    fn move_right(&mut self) {
        self.reset_sticky_state();
        let line_len = self.current_line().len();
        if self.cursor_col < line_len {
            let bounds = self.grapheme_boundaries();
            for &b in &bounds {
                if b > self.cursor_col {
                    self.cursor_col = b;
                    return;
                }
            }
        } else if self.cursor_line < self.lines.len() - 1 {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }

    fn word_boundary_left(&self) -> usize {
        word_left(
            self.current_line(),
            self.cursor_col,
            self.word_classifier.as_ref(),
        )
    }

    fn word_boundary_right(&self) -> usize {
        word_right(
            self.current_line(),
            self.cursor_col,
            self.word_classifier.as_ref(),
        )
    }

    /// Moves one word left, wrapping to the end of the previous line at column
    /// zero.
    fn move_word_left(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.cursor_line -= 1;
                self.cursor_col = self.current_line().len();
            }
            return;
        }
        self.cursor_col = self.word_boundary_left();
    }

    /// Moves one word right, wrapping to the start of the next line at
    /// end-of-line.
    fn move_word_right(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col >= line_len {
            if self.cursor_line + 1 < self.lines.len() {
                self.cursor_line += 1;
                self.cursor_col = 0;
            }
            return;
        }
        self.cursor_col = self.word_boundary_right();
    }

    // -- Wrapping --

    /// Atomic display segments of `line`. One grapheme per segment.
    ///
    /// A named seam even though it is a plain grapheme walk today: the wrapper
    /// and the vertical-move snap consume segments as indivisible units, which
    /// is where marker-style atoms would slot in.
    fn segment_line<'a>(&self, line: &'a str) -> Vec<AtomicSegment<'a>> {
        line.grapheme_indices(true)
            .map(|(i, g)| AtomicSegment {
                text: g,
                start_index: i,
            })
            .collect()
    }

    /// Greedy word-wrap of `line` at `width`, returning `(start, end)` byte
    /// spans whose concatenation reconstructs `line` exactly.
    ///
    /// The walk counts display columns and remembers the last
    /// whitespace-to-non-whitespace transition as a wrap opportunity. On
    /// overflow it backtracks to that opportunity when the run since it still
    /// fits, otherwise force-breaks at the current segment. A single segment
    /// wider than `width` (a wide grapheme) stays whole on its own row.
    fn wrap_line_spans(&self, line: &str, width: usize) -> Vec<(usize, usize)> {
        let width = width.max(1);
        let segments = self.segment_line(line);

        let mut chunks: Vec<(usize, usize)> = Vec::new();
        let mut current_width: usize = 0;
        let mut chunk_start: usize = 0;

        // Byte offset and running width at the last wrap opportunity.
        let mut wrap_opp_index: Option<usize> = None;
        let mut wrap_opp_width: usize = 0;

        for (i, seg) in segments.iter().enumerate() {
            let char_index = seg.start_index;
            let grapheme = seg.text;
            let g_width = self.measure(grapheme);
            let is_ws = is_segment_whitespace(grapheme);

            // Overflow check runs before advancing the running width.
            if current_width + g_width > width {
                // Backtrack to the last opportunity when the trailing run since
                // it, plus this segment, still fits. Otherwise force-break here.
                let backtrack =
                    wrap_opp_index.filter(|_| current_width + g_width - wrap_opp_width <= width);
                if let Some(opp) = backtrack {
                    chunks.push((chunk_start, opp));
                    chunk_start = opp;
                    current_width -= wrap_opp_width;
                } else if chunk_start < char_index {
                    chunks.push((chunk_start, char_index));
                    chunk_start = char_index;
                    current_width = 0;
                }
                wrap_opp_index = None;
            }

            // A segment wider than the whole width cannot be split further (a
            // wide grapheme), so it starts its own chunk and the next segment
            // begins a fresh row after it.
            if g_width > width {
                chunk_start = char_index;
                current_width = g_width;
                wrap_opp_index = None;
                continue;
            }

            current_width += g_width;

            // A wrap opportunity is the boundary after a whitespace run, right
            // before the next non-whitespace segment.
            if is_ws
                && let Some(next) = segments.get(i + 1)
                && !is_segment_whitespace(next.text)
            {
                wrap_opp_index = Some(next.start_index);
                wrap_opp_width = current_width;
            }
        }

        chunks.push((chunk_start, line.len()));
        chunks
    }

    /// Builds the visual-line map for the whole document at `width`.
    ///
    /// An empty logical line yields one zero-length visual line. A line that
    /// fits yields one visual line spanning its content. Wider lines are
    /// word-wrapped.
    fn build_visual_line_map(&self, width: usize) -> Vec<VisualLine> {
        let width = width.max(1);
        let mut visual_lines: Vec<VisualLine> = Vec::new();

        for (i, line) in self.lines.iter().enumerate() {
            if line.is_empty() {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: 0,
                });
                continue;
            }
            if self.measure(line) <= width {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: 0,
                    length: line.len(),
                });
                continue;
            }
            for (start, end) in self.wrap_line_spans(line, width) {
                visual_lines.push(VisualLine {
                    logical_line: i,
                    start_col: start,
                    length: end - start,
                });
            }
        }

        visual_lines
    }

    /// Index into `vls` of the visual line containing `(line, col)`. For the
    /// last segment of a logical line, a cursor exactly at `length` (end of
    /// line) counts as contained. Falls back to the last visual line if nothing
    /// matches (a stale-map guard, not a hot path).
    fn find_visual_line_at(&self, vls: &[VisualLine], line: usize, col: usize) -> usize {
        for (i, vl) in vls.iter().enumerate() {
            if vl.logical_line != line {
                continue;
            }
            let offset = col.saturating_sub(vl.start_col);
            let is_last_segment_of_line = i == vls.len() - 1 || vls[i + 1].logical_line != line;
            if col >= vl.start_col
                && (offset < vl.length || (is_last_segment_of_line && offset == vl.length))
            {
                return i;
            }
        }
        vls.len().saturating_sub(1)
    }

    fn find_current_visual_line(&self, vls: &[VisualLine]) -> usize {
        self.find_visual_line_at(vls, self.cursor_line, self.cursor_col)
    }

    /// Clears every piece of sticky-column state.
    ///
    /// Called from any action that is not a vertical cursor move (edits,
    /// horizontal moves, line-endpoint jumps) so the next vertical move captures
    /// a fresh anchor. One site so a new horizontal-ish operation only needs to
    /// touch one name to behave correctly.
    fn reset_sticky_state(&mut self) {
        self.preferred_visual_col = None;
        self.snapped_from_cursor_col = None;
    }

    /// Applies the sticky-column decision table and returns the byte column into
    /// the target visual line where the next vertical move should land.
    ///
    /// | P | S | T | U | Scenario                                             | Set Preferred | Move To     |
    /// |---|---|---|---| ---------------------------------------------------- |---------------|-------------|
    /// | 0 | * | 0 | - | Start nav, target fits                               | null          | current     |
    /// | 0 | * | 1 | - | Start nav, target shorter                            | current       | target end  |
    /// | 1 | 0 | 0 | 0 | Clamped, target fits preferred                       | null          | preferred   |
    /// | 1 | 0 | 0 | 1 | Clamped, target longer but still can't fit preferred | keep          | target end  |
    /// | 1 | 0 | 1 | - | Clamped, target even shorter                         | keep          | target end  |
    /// | 1 | 1 | 0 | - | Rewrapped, target fits current                       | null          | current     |
    /// | 1 | 1 | 1 | - | Rewrapped, target shorter than current               | current       | target end  |
    ///
    /// Where P = preferred col is set, S = cursor in middle of source line, T =
    /// target shorter than current visual col, U = target shorter than
    /// preferred col.
    fn compute_vertical_move_column(
        &mut self,
        current_visual_col: usize,
        source_max_visual_col: usize,
        target_max_visual_col: usize,
    ) -> usize {
        let has_preferred = self.preferred_visual_col.is_some();
        let cursor_in_middle = current_visual_col < source_max_visual_col;
        let target_too_short = target_max_visual_col < current_visual_col;

        if !has_preferred || cursor_in_middle {
            if target_too_short {
                // Cases 2 and 7: remember where we wanted to be, land at the
                // target's short end.
                self.preferred_visual_col = Some(current_visual_col);
                return target_max_visual_col;
            }
            // Cases 1 and 6: clear preferred only. We leave
            // `snapped_from_cursor_col` alone because the caller's snap scan may
            // set it, and even if not, a following vertical move still needs the
            // pre-snap anchor from the previous step.
            self.preferred_visual_col = None;
            return current_visual_col;
        }

        let preferred = self.preferred_visual_col.expect("has_preferred checked");
        let target_cant_fit_preferred = target_max_visual_col < preferred;
        if target_too_short || target_cant_fit_preferred {
            // Cases 4 and 5: keep preferred, land at the target's end.
            return target_max_visual_col;
        }

        // Case 3: land exactly on preferred, then clear it.
        self.preferred_visual_col = None;
        preferred
    }

    /// Moves the cursor to `target_visual_line`, honoring the sticky column and
    /// snapping onto an atomic segment (a multi-byte grapheme) the byte-column
    /// math would otherwise land inside.
    fn move_to_visual_line(
        &mut self,
        vls: &[VisualLine],
        current_visual_line: usize,
        target_visual_line: usize,
    ) {
        let Some(current_vl) = vls.get(current_visual_line).copied() else {
            return;
        };
        let Some(target_vl) = vls.get(target_visual_line).copied() else {
            return;
        };

        // Source visual column: use the pre-snap position when the last vertical
        // move snapped onto an atomic segment, so the decision table sees the
        // user's original intent rather than the snapped offset.
        let current_visual_col = if let Some(snapped) = self.snapped_from_cursor_col {
            let vl_idx = self.find_visual_line_at(vls, current_vl.logical_line, snapped);
            snapped.saturating_sub(vls[vl_idx].start_col)
        } else {
            self.cursor_col.saturating_sub(current_vl.start_col)
        };

        // Max columns: on a non-last segment of a logical line the cursor
        // cannot sit past `length - 1`, since that position belongs to the next
        // visual line. On the final segment it can sit at `length` (end of
        // line).
        let is_last_source_segment = current_visual_line == vls.len() - 1
            || vls[current_visual_line + 1].logical_line != current_vl.logical_line;
        let source_max_visual_col = if is_last_source_segment {
            current_vl.length
        } else {
            current_vl.length.saturating_sub(1)
        };

        let is_last_target_segment = target_visual_line == vls.len() - 1
            || vls[target_visual_line + 1].logical_line != target_vl.logical_line;
        let target_max_visual_col = if is_last_target_segment {
            target_vl.length
        } else {
            target_vl.length.saturating_sub(1)
        };

        let move_to_visual_col = self.compute_vertical_move_column(
            current_visual_col,
            source_max_visual_col,
            target_max_visual_col,
        );

        self.cursor_line = target_vl.logical_line;
        let target_col = target_vl.start_col + move_to_visual_col;
        let logical_len = self.lines[target_vl.logical_line].len();
        self.cursor_col = target_col.min(logical_len);

        // Atomic-segment snap: if the cursor landed inside a multi-byte segment,
        // snap it to the segment start so it never sits mid-grapheme. When
        // moving down into a continuation visual line of a segment that began on
        // an earlier row, skip forward past the continuation rows so the cursor
        // keeps making progress.
        let logical_line = self.lines[target_vl.logical_line].clone();
        let segments = self.segment_line(&logical_line);
        for seg in &segments {
            if seg.start_index > self.cursor_col {
                break;
            }
            if seg.text.len() <= 1 {
                continue;
            }
            let seg_end = seg.start_index + seg.text.len();
            if self.cursor_col >= seg_end {
                continue;
            }
            // Cursor is strictly inside a multi-byte segment.
            let is_continuation = seg.start_index < target_vl.start_col;
            let is_moving_down = target_visual_line > current_visual_line;

            if is_continuation && is_moving_down {
                let mut next = target_visual_line + 1;
                while next < vls.len()
                    && vls[next].logical_line == target_vl.logical_line
                    && vls[next].start_col < seg_end
                {
                    next += 1;
                }
                if next < vls.len() {
                    self.move_to_visual_line(vls, current_visual_line, next);
                    return;
                }
            }

            // Snap to the segment start, recording the pre-snap cursor so the
            // next vertical move resolves the sticky column against it.
            self.snapped_from_cursor_col = Some(self.cursor_col);
            self.cursor_col = seg.start_index;
            return;
        }

        // No snap: we exited whatever segment we were on.
        self.snapped_from_cursor_col = None;
    }

    /// Moves the cursor up one visual line.
    fn move_up(&mut self) {
        let vls = self.build_visual_line_map(self.layout_width);
        if vls.is_empty() {
            return;
        }
        let current = self.find_current_visual_line(&vls);
        if current == 0 {
            return;
        }
        self.move_to_visual_line(&vls, current, current - 1);
    }

    /// Moves the cursor down one visual line.
    fn move_down(&mut self) {
        let vls = self.build_visual_line_map(self.layout_width);
        if vls.is_empty() {
            return;
        }
        let current = self.find_current_visual_line(&vls);
        if current + 1 >= vls.len() {
            return;
        }
        self.move_to_visual_line(&vls, current, current + 1);
    }

    /// Moves the cursor one page up (`direction < 0`) or down within the
    /// visual-line map. The page size is the last-drawn visible window. The
    /// target row is clamped so paging past an edge is a no-op.
    fn page_scroll(&mut self, direction: i32) {
        self.last_action = LastAction::None;
        let vls = self.build_visual_line_map(self.layout_width);
        if vls.is_empty() {
            return;
        }
        let page_size = self.last_visible_rows.max(1);
        let current = self.find_current_visual_line(&vls);
        let target = if direction < 0 {
            current.saturating_sub(page_size)
        } else {
            (current + page_size).min(vls.len() - 1)
        };
        if target == current {
            return;
        }
        self.move_to_visual_line(&vls, current, target);
    }

    // -- Insert / delete --

    /// Inserts a newline at the cursor, splitting the current line.
    fn insert_newline_internal(&mut self) {
        let rest = self.lines[self.cursor_line][self.cursor_col..].to_string();
        self.lines[self.cursor_line].truncate(self.cursor_col);
        self.cursor_line += 1;
        self.lines.insert(self.cursor_line, rest);
        self.cursor_col = 0;
    }

    /// Inserts one character at the cursor with fish-style undo coalescing.
    fn insert_char(&mut self, c: char) {
        // Coalescing rule: consecutive non-whitespace characters share one undo
        // unit, while every whitespace character pushes its own snapshot. The
        // word after whitespace does not push a fresh snapshot because
        // `last_action` stays `TypeWord` through the space, so undoing "hello
        // world" takes two steps (" world", then "hello").
        if c.is_whitespace() || self.last_action != LastAction::TypeWord {
            self.save_undo();
        }
        self.lines[self.cursor_line].insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
        self.last_action = LastAction::TypeWord;
        self.reset_sticky_state();
    }

    /// Deletes one grapheme backward, merging with the previous line at column
    /// zero.
    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.save_undo();
            let old_col = self.cursor_col;
            self.move_left();
            self.lines[self.cursor_line].drain(self.cursor_col..old_col);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
        } else if self.cursor_line > 0 {
            self.save_undo();
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
        }
    }

    /// Deletes one grapheme forward, merging with the next line at end-of-line.
    fn delete_forward(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col < line_len {
            self.save_undo();
            let bounds = self.grapheme_boundaries();
            let next = bounds
                .iter()
                .find(|&&b| b > self.cursor_col)
                .copied()
                .unwrap_or(line_len);
            self.lines[self.cursor_line].drain(self.cursor_col..next);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
        } else if self.cursor_line < self.lines.len() - 1 {
            self.save_undo();
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next);
            self.last_action = LastAction::None;
            self.reset_sticky_state();
        }
    }

    // -- Kill ring --
    //
    // Backward kills prepend to the current ring entry, forward kills append.
    // Consecutive kills accumulate into one entry when the previous action was
    // also a kill.

    /// Kills from the cursor to end of line, or the newline when already there.
    fn kill_to_end(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col >= line_len {
            if self.cursor_line < self.lines.len() - 1 {
                self.save_undo();
                let next = self.lines.remove(self.cursor_line + 1);
                self.kill_ring
                    .push("\n", false, self.last_action == LastAction::Kill);
                self.lines[self.cursor_line].push_str(&next);
                self.last_action = LastAction::Kill;
            }
            return;
        }
        self.save_undo();
        let deleted: String = self.lines[self.cursor_line]
            .drain(self.cursor_col..)
            .collect();
        self.kill_ring
            .push(&deleted, false, self.last_action == LastAction::Kill);
        self.last_action = LastAction::Kill;
    }

    /// Kills from the cursor to start of line, or merges with the previous line
    /// when already there.
    fn kill_to_start(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_line > 0 {
                self.save_undo();
                let current = self.lines.remove(self.cursor_line);
                self.cursor_line -= 1;
                self.cursor_col = self.lines[self.cursor_line].len();
                self.kill_ring
                    .push("\n", true, self.last_action == LastAction::Kill);
                self.lines[self.cursor_line].push_str(&current);
                self.last_action = LastAction::Kill;
            }
            return;
        }
        self.save_undo();
        let deleted: String = self.lines[self.cursor_line]
            .drain(..self.cursor_col)
            .collect();
        self.kill_ring
            .push(&deleted, true, self.last_action == LastAction::Kill);
        self.cursor_col = 0;
        self.last_action = LastAction::Kill;
    }

    /// Kills the word before the cursor, or merges with the previous line at
    /// column zero.
    fn kill_word_backward(&mut self) {
        if self.cursor_col == 0 {
            if self.cursor_line == 0 {
                return;
            }
            self.save_undo();
            let current = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&current);
            self.kill_ring
                .push("\n", true, self.last_action == LastAction::Kill);
            self.last_action = LastAction::Kill;
            return;
        }
        let target = self.word_boundary_left();
        if target == self.cursor_col {
            return;
        }
        self.save_undo();
        let deleted: String = self.lines[self.cursor_line]
            .drain(target..self.cursor_col)
            .collect();
        self.kill_ring
            .push(&deleted, true, self.last_action == LastAction::Kill);
        self.cursor_col = target;
        self.last_action = LastAction::Kill;
    }

    /// Kills the word after the cursor, or merges the next line at end-of-line.
    fn kill_word_forward(&mut self) {
        let line_len = self.current_line().len();
        if self.cursor_col >= line_len {
            if self.cursor_line + 1 >= self.lines.len() {
                return;
            }
            self.save_undo();
            let next = self.lines.remove(self.cursor_line + 1);
            self.lines[self.cursor_line].push_str(&next);
            self.kill_ring
                .push("\n", false, self.last_action == LastAction::Kill);
            self.last_action = LastAction::Kill;
            return;
        }
        let target = self.word_boundary_right();
        if target == self.cursor_col {
            return;
        }
        self.save_undo();
        let deleted: String = self.lines[self.cursor_line]
            .drain(self.cursor_col..target)
            .collect();
        self.kill_ring
            .push(&deleted, false, self.last_action == LastAction::Kill);
        self.last_action = LastAction::Kill;
    }

    /// Yanks the most recent kill-ring entry at the cursor.
    fn yank(&mut self) {
        if let Some(text) = self.kill_ring.peek().map(str::to_string) {
            self.save_undo();
            self.insert_yanked_text(&text);
            self.last_action = LastAction::Yank;
            self.reset_sticky_state();
        }
    }

    /// Cycles the kill ring: replaces the just-yanked text with the next entry.
    ///
    /// Only valid immediately after a yank or another yank-pop, and a no-op with
    /// fewer than two entries.
    fn yank_pop(&mut self) {
        if self.last_action != LastAction::Yank || self.kill_ring.len() <= 1 {
            return;
        }
        // The yanked text is still at the ring head (rotate has not run yet), so
        // it tells us what to delete.
        let previous = match self.kill_ring.peek() {
            Some(s) => s.to_string(),
            None => return,
        };
        self.save_undo();
        self.delete_yanked_text(&previous);
        self.kill_ring.rotate();
        if let Some(next) = self.kill_ring.peek().map(str::to_string) {
            self.insert_yanked_text(&next);
        }
        self.last_action = LastAction::Yank;
        self.reset_sticky_state();
    }

    /// Inserts `text` at the cursor, handling embedded newlines, leaving the
    /// cursor just after the inserted region. Shared by yank and yank-pop.
    fn insert_yanked_text(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.insert_newline_internal();
            } else {
                self.lines[self.cursor_line].insert(self.cursor_col, ch);
                self.cursor_col += ch.len_utf8();
            }
        }
    }

    /// Removes `text` ending at the cursor, reversing the last yank so yank-pop
    /// can replace it without disturbing surrounding content.
    fn delete_yanked_text(&mut self, text: &str) {
        let yank_lines: Vec<&str> = text.split('\n').collect();
        if yank_lines.len() == 1 {
            let byte_len = text.len();
            let start_col = self.cursor_col.saturating_sub(byte_len);
            self.lines[self.cursor_line].drain(start_col..self.cursor_col);
            self.cursor_col = start_col;
            return;
        }

        // Multi-line: the cursor is at the end of the last yanked line. The yank
        // started `n - 1` lines up, at the column where `yank_lines[0]` was
        // appended to that line's original content.
        let n = yank_lines.len();
        let start_line = self.cursor_line.saturating_sub(n - 1);
        let first_yank = yank_lines.first().copied().unwrap_or("");
        let start_col = self.lines[start_line]
            .len()
            .saturating_sub(first_yank.len());

        let before: String = self.lines[start_line][..start_col].to_string();
        let after: String = self.lines[self.cursor_line][self.cursor_col..].to_string();

        self.lines.drain(start_line..=self.cursor_line);
        self.lines.insert(start_line, before.clone() + &after);

        self.cursor_line = start_line;
        self.cursor_col = before.len();
    }

    // -- History navigation --

    /// Navigates history upward.
    ///
    /// The first call (not yet browsing) saves an undo snapshot and parks the
    /// current text as a draft at the tail of the ring. That draft is what
    /// [`TextArea::history_down`] returns to when walking past the newest real
    /// entry. Later calls do not push further snapshots, so one undo returns to
    /// the pre-browse state no matter how far the user walked.
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let idx = match self.history_index {
            None => {
                self.save_undo();
                self.history.push(self.text());
                self.history.len() - 2
            }
            Some(i) if i > 0 => i - 1,
            _ => return,
        };
        self.history_index = Some(idx);
        self.load_history_entry(idx);
    }

    /// Navigates history downward, restoring and dropping the draft when it
    /// walks back past the newest real entry.
    fn history_down(&mut self) {
        let idx = match self.history_index {
            Some(i) => i + 1,
            None => return,
        };
        if idx >= self.history.len() {
            return;
        }
        self.history_index = if idx == self.history.len() - 1 {
            None
        } else {
            Some(idx)
        };
        let text = self.history[idx].clone();
        if self.history_index.is_none() {
            self.history.pop();
        }
        self.set_document(&text);
    }

    fn load_history_entry(&mut self, idx: usize) {
        let text = self.history[idx].clone();
        self.set_document(&text);
    }

    /// Replaces the document with `text` (already normalized), cursor to end.
    /// Used by history navigation, which owns the ring shape itself.
    fn set_document(&mut self, text: &str) {
        self.lines = if text.is_empty() {
            vec![String::new()]
        } else {
            text.split('\n').map(str::to_string).collect()
        };
        self.cursor_line = self.lines.len() - 1;
        self.cursor_col = self.lines[self.cursor_line].len();
    }

    // -- Submit --

    /// Whether the character immediately before the cursor is a literal `\`.
    fn cursor_preceded_by_backslash(&self) -> bool {
        if self.cursor_col == 0 {
            return false;
        }
        self.current_line()[..self.cursor_col].chars().next_back() == Some('\\')
    }

    /// Whether the cursor sits on the first line at an empty-or-`/`-only
    /// prefix. Restricts the palette trigger to the start of a message.
    fn is_at_start_of_message(&self) -> bool {
        if self.cursor_line != 0 {
            return false;
        }
        let trimmed = self.lines[0][..self.cursor_col].trim();
        trimmed.is_empty() || trimmed == "/"
    }

    /// Fires `on_submit` with the trimmed contents, records the submitted text
    /// for polling, then resets to an empty document with a clean undo stack.
    fn submit_value(&mut self, ctx: &mut EventContext) {
        let text = self.text().trim().to_string();
        self.submitted_text = Some(text.clone());
        if let Some(cb) = self.on_submit.as_mut() {
            cb(ctx, &text);
        }
        self.lines = vec![String::new()];
        self.cursor_line = 0;
        self.cursor_col = 0;
        self.reset_sticky_state();
        self.undo_stack.clear();
        self.last_action = LastAction::None;
        self.history_index = None;
    }

    /// Consumes the event with a redraw, then fires `on_change` if the text
    /// actually changed since the last check.
    ///
    /// Every handled key routes through here so cursor-only moves still redraw
    /// but do not fire `on_change`. Programmatic setters bypass it, so
    /// `on_change` reflects interactive edits only.
    fn check_changed(&mut self, ctx: &mut EventContext) {
        ctx.consume_and_redraw();
        if self.on_change.is_none() {
            return;
        }
        let new = self.text();
        if new != self.previous_val {
            if let Some(cb) = self.on_change.as_mut() {
                cb(ctx, &new);
            }
        }
        self.previous_val = new;
    }

    // -- Vertical dispatch helpers --

    /// Handles an Up press: history at an empty or already-browsing top line,
    /// jump-to-start on an otherwise top line, else move up one visual line.
    fn on_cursor_up(&mut self) {
        let vls = self.build_visual_line_map(self.layout_width);
        let current_vl = self.find_current_visual_line(&vls);
        if self.is_empty_doc() {
            self.history_up();
        } else if self.history_index.is_some() && current_vl == 0 {
            self.history_up();
        } else if current_vl == 0 {
            self.cursor_col = 0;
            self.reset_sticky_state();
        } else {
            self.move_up();
        }
        self.last_action = LastAction::None;
    }

    /// Handles a Down press: symmetric to [`TextArea::on_cursor_up`].
    fn on_cursor_down(&mut self) {
        let vls = self.build_visual_line_map(self.layout_width);
        let current_vl = self.find_current_visual_line(&vls);
        let on_last_vl = current_vl + 1 >= vls.len();
        if self.history_index.is_some() && on_last_vl {
            self.history_down();
        } else if on_last_vl {
            self.cursor_col = self.current_line().len();
            self.reset_sticky_state();
        } else {
            self.move_down();
        }
        self.last_action = LastAction::None;
    }

    // -- Drawing --

    /// Writes one border rule into `surf` at `row`: `inlay` text followed by `─`
    /// padding to `width`, all in `style`, truncated to `width`.
    fn draw_rule(&self, surf: &mut Surface, row: u16, width: u16, inlay: &str, style: Style) {
        let width_usize = usize::from(width);
        let mut s = String::from(inlay);
        let inlay_w = self.measure(inlay);
        for _ in inlay_w..width_usize {
            s.push('─');
        }
        let mut col: u16 = 0;
        for g in s.graphemes(true) {
            let gw = gwidth::gwidth(g, self.width_method);
            if usize::from(col) + usize::from(gw) > width_usize {
                break;
            }
            surf.write_cell(
                col,
                row,
                Cell {
                    char: Character::new(g, u8::try_from(gw).expect("grapheme width fits a u8")),
                    style,
                    ..Cell::default()
                },
            );
            col = col.saturating_add(gw);
        }
    }
}

impl Widget for TextArea {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let width = ctx
            .max
            .width
            .expect("TextArea requires a bounded max width");
        self.width_method = ctx.width_method;

        let width_usize = usize::from(width);
        let content_width = width_usize.saturating_sub(self.padding_x * 2);
        let border_style = Style {
            fg: self.theme.border_color,
            ..Style::default()
        };

        if content_width == 0 {
            // No room for content: draw just the two border rules.
            let mut surf = Surface::with_size(Size { width, height: 2 });
            self.draw_rule(&mut surf, 0, width, "", border_style);
            self.draw_rule(&mut surf, 1, width, "", border_style);
            return surf;
        }

        // Reserve one column for the caret at end-of-line when there is no
        // padding, so a line that would exactly fill the content still leaves
        // the caret on screen.
        let layout_width = if self.padding_x == 0 {
            content_width.saturating_sub(1).max(1)
        } else {
            content_width
        };
        self.layout_width = layout_width;

        // Visible-row cap from the constraints. The layout owns the height
        // policy; we only clamp to what we are given.
        let available = match ctx.max.height {
            Some(h) => usize::from(h).saturating_sub(2),
            None => usize::MAX,
        };
        let cap = self
            .max_visible_rows
            .map(|m| m.max(1))
            .unwrap_or(available)
            .min(available);

        let vls = self.build_visual_line_map(layout_width);
        let total_visual = vls.len();
        let cursor_vl_idx = self.find_current_visual_line(&vls);
        let visible_count = total_visual.min(cap);
        self.last_visible_rows = visible_count;

        // Scroll window: keep the cursor's visual line inside it, then clamp so
        // we never scroll past the last row.
        let mut scroll_start = if cursor_vl_idx < self.scroll_offset {
            cursor_vl_idx
        } else if visible_count > 0 && cursor_vl_idx >= self.scroll_offset + visible_count {
            cursor_vl_idx + 1 - visible_count
        } else {
            self.scroll_offset
        };
        scroll_start = scroll_start.min(total_visual.saturating_sub(visible_count));
        self.scroll_offset = scroll_start;

        let height = u16::try_from(visible_count + 2).expect("editor height fits a u16");
        let mut surf = Surface::with_size(Size { width, height });

        // Top rule with optional scroll indicator and top-bar label.
        let mut top_inlay = String::new();
        if scroll_start > 0 {
            top_inlay.push_str(&format!("─── ↑ {scroll_start} more "));
        }
        if let Some(label) = &self.top_bar_label {
            top_inlay.push_str(&format!("─── {label} "));
        }
        self.draw_rule(&mut surf, 0, width, &top_inlay, border_style);

        // Content rows.
        let padding_col = u16::try_from(self.padding_x).expect("padding fits a u16");
        for (offset, vl_idx) in (scroll_start..scroll_start + visible_count).enumerate() {
            let row = u16::try_from(offset + 1).expect("row fits a u16");
            let vl = vls[vl_idx];
            let line = &self.lines[vl.logical_line];
            let text = &line[vl.start_col..vl.start_col + vl.length];
            let mut col = padding_col;
            for g in text.graphemes(true) {
                let gw = gwidth::gwidth(g, self.width_method);
                if usize::from(col) + usize::from(gw) > width_usize {
                    break;
                }
                surf.write_cell(
                    col,
                    row,
                    Cell {
                        char: Character::new(
                            g,
                            u8::try_from(gw).expect("grapheme width fits a u8"),
                        ),
                        style: Style::default(),
                        ..Cell::default()
                    },
                );
                col = col.saturating_add(gw);
            }
        }

        // Bottom rule with optional `↓ N more` indicator.
        let bottom_row = u16::try_from(visible_count + 1).expect("row fits a u16");
        let lines_below = total_visual.saturating_sub(scroll_start + visible_count);
        let mut bottom_inlay = String::new();
        if lines_below > 0 {
            bottom_inlay.push_str(&format!("─── ↓ {lines_below} more "));
        }
        self.draw_rule(&mut surf, bottom_row, width, &bottom_inlay, border_style);

        // Report the caret when its visual line is inside the window. The
        // framework renders it only while this widget is focused.
        if visible_count > 0
            && cursor_vl_idx >= scroll_start
            && cursor_vl_idx < scroll_start + visible_count
        {
            let vl = vls[cursor_vl_idx];
            debug_assert_eq!(vl.logical_line, self.cursor_line);
            let before = &self.lines[self.cursor_line][vl.start_col..self.cursor_col];
            let cursor_vis_col = self.measure(before);
            let row = u16::try_from(cursor_vl_idx - scroll_start + 1).expect("cursor row fits u16");
            let col = u16::try_from(self.padding_x + cursor_vis_col).expect("cursor col fits u16");
            surf.cursor = Some(CursorState {
                row,
                col,
                shape: CursorShape::Default,
            });
        }

        surf
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::FocusIn | Event::FocusOut => ctx.redraw = true,
            Event::Paste(text) => {
                self.insert_at_cursor(text);
                self.check_changed(ctx);
            }
            Event::KeyPress(key) => self.handle_key(ctx, key),
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }

    fn debug_label(&self) -> &'static str {
        "TextArea"
    }
}

impl TextArea {
    /// Dispatches a key press. The order matters: newline is checked before
    /// submit, word motions before plain motions, and word deletes before char
    /// deletes, so a modified key never falls through to the plainer chord.
    fn handle_key(&mut self, ctx: &mut EventContext, key: &Key) {
        let mods = key.mods;
        let empty = Modifiers::empty();
        let ctrl = Modifiers::CTRL;
        let alt = Modifiers::ALT;

        // Undo.
        if key.matches(u32::from('_'), ctrl)
            || key.matches(u32::from('-'), ctrl)
            || key.matches(u32::from('/'), ctrl)
            || key.matches(u32::from('z'), ctrl)
        {
            self.restore_undo();
            self.check_changed(ctx);
            return;
        }

        // Newline, checked before submit so Shift+Enter and friends never
        // submit.
        let is_newline = key.matches(Key::ENTER, Modifiers::SHIFT)
            || key.matches(Key::ENTER, alt)
            || key.matches(u32::from('j'), ctrl)
            || key.matches(0x0A, empty);
        if is_newline {
            self.save_undo();
            self.insert_newline_internal();
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }

        // Submit (plain Enter). A `\` immediately before the cursor inserts a
        // newline instead, the standard multi-line escape hatch.
        if key.matches(Key::ENTER, empty) {
            if !self.submit_enabled {
                ctx.consume_event();
                return;
            }
            if self.cursor_preceded_by_backslash() {
                self.save_undo();
                self.lines[self.cursor_line].remove(self.cursor_col - 1);
                self.cursor_col -= 1;
                self.insert_newline_internal();
                self.reset_sticky_state();
                self.last_action = LastAction::None;
                self.check_changed(ctx);
                return;
            }
            self.submit_value(ctx);
            self.check_changed(ctx);
            return;
        }

        // Vertical movement (history-aware).
        if key.matches(Key::UP, empty) || key.matches(u32::from('p'), ctrl) {
            self.on_cursor_up();
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::DOWN, empty) || key.matches(u32::from('n'), ctrl) {
            self.on_cursor_down();
            self.check_changed(ctx);
            return;
        }

        // Page up/down.
        if key.matches(Key::PAGE_UP, empty) {
            self.page_scroll(-1);
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::PAGE_DOWN, empty) {
            self.page_scroll(1);
            self.check_changed(ctx);
            return;
        }

        // Word movement, before plain movement.
        if key.matches(u32::from('b'), alt)
            || key.matches(Key::LEFT, alt)
            || key.matches(Key::LEFT, ctrl)
        {
            self.move_word_left();
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('f'), alt)
            || key.matches(Key::RIGHT, alt)
            || key.matches(Key::RIGHT, ctrl)
        {
            self.move_word_right();
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }

        // Plain horizontal movement.
        if key.matches(Key::LEFT, empty) || key.matches(u32::from('b'), ctrl) {
            self.move_left();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::RIGHT, empty) || key.matches(u32::from('f'), ctrl) {
            self.move_right();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('a'), ctrl) || key.matches(Key::HOME, empty) {
            self.cursor_col = 0;
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('e'), ctrl) || key.matches(Key::END, empty) {
            self.cursor_col = self.current_line().len();
            self.reset_sticky_state();
            self.last_action = LastAction::None;
            self.check_changed(ctx);
            return;
        }

        // Deletion, word-level before char-level.
        if key.matches(u32::from('w'), ctrl) || key.matches(Key::BACKSPACE, alt) {
            self.kill_word_backward();
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('d'), alt) {
            self.kill_word_forward();
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::BACKSPACE, empty) {
            self.backspace();
            self.check_changed(ctx);
            return;
        }
        if key.matches(Key::DELETE, empty) || key.matches(u32::from('d'), ctrl) {
            self.delete_forward();
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('u'), ctrl) {
            self.kill_to_start();
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('k'), ctrl) {
            self.kill_to_end();
            self.check_changed(ctx);
            return;
        }

        // Yank.
        if key.matches(u32::from('y'), ctrl) {
            self.yank();
            self.check_changed(ctx);
            return;
        }
        if key.matches(u32::from('y'), alt) {
            self.yank_pop();
            self.check_changed(ctx);
            return;
        }

        // Character insertion. Only printable text with no modifier beyond
        // Shift inserts, so Ctrl/Alt combos stay bindings.
        if (mods - Modifiers::SHIFT).is_empty()
            && let Some(text) = key.text.as_deref()
        {
            // Palette trigger: a plain `/` at an empty start-of-message fires
            // the callback and swallows the `/`.
            if text == "/" && self.on_palette_trigger.is_some() && self.is_at_start_of_message() {
                if let Some(cb) = self.on_palette_trigger.as_mut() {
                    cb(ctx);
                }
                ctx.consume_and_redraw();
                return;
            }
            let mut inserted = false;
            for c in text.chars() {
                if !c.is_control() {
                    self.insert_char(c);
                    inserted = true;
                }
            }
            if inserted {
                self.check_changed(ctx);
            }
        }
    }
}

/// Whether `seg` counts as whitespace for a wrap opportunity: non-empty and
/// every scalar is whitespace. A multi-scalar atom is whitespace only when all
/// of it is, so an atom carrying both a space and a non-space is not.
fn is_segment_whitespace(seg: &str) -> bool {
    !seg.is_empty() && seg.chars().all(char::is_whitespace)
}

/// The fixed editing chords. See [`TextArea::bindings`].
static BINDINGS: [ChordDoc; 21] = [
    ChordDoc {
        keys: "↑ / Ctrl-P",
        description: "Move up, or previous history at the top line",
        group: "Movement",
    },
    ChordDoc {
        keys: "↓ / Ctrl-N",
        description: "Move down, or next history at the bottom line",
        group: "Movement",
    },
    ChordDoc {
        keys: "← / Ctrl-B",
        description: "Move left",
        group: "Movement",
    },
    ChordDoc {
        keys: "→ / Ctrl-F",
        description: "Move right",
        group: "Movement",
    },
    ChordDoc {
        keys: "Alt-B / Ctrl-←",
        description: "Move one word left",
        group: "Movement",
    },
    ChordDoc {
        keys: "Alt-F / Ctrl-→",
        description: "Move one word right",
        group: "Movement",
    },
    ChordDoc {
        keys: "Ctrl-A / Home",
        description: "Move to line start",
        group: "Movement",
    },
    ChordDoc {
        keys: "Ctrl-E / End",
        description: "Move to line end",
        group: "Movement",
    },
    ChordDoc {
        keys: "PageUp",
        description: "Scroll up one page",
        group: "Movement",
    },
    ChordDoc {
        keys: "PageDown",
        description: "Scroll down one page",
        group: "Movement",
    },
    ChordDoc {
        keys: "Backspace",
        description: "Delete the character before the cursor",
        group: "Editing",
    },
    ChordDoc {
        keys: "Ctrl-D / Delete",
        description: "Delete the character after the cursor",
        group: "Editing",
    },
    ChordDoc {
        keys: "Ctrl-W / Alt-Backspace",
        description: "Delete the word before the cursor",
        group: "Editing",
    },
    ChordDoc {
        keys: "Alt-D",
        description: "Delete the word after the cursor",
        group: "Editing",
    },
    ChordDoc {
        keys: "Ctrl-U",
        description: "Kill to line start",
        group: "Kill ring",
    },
    ChordDoc {
        keys: "Ctrl-K",
        description: "Kill to line end",
        group: "Kill ring",
    },
    ChordDoc {
        keys: "Ctrl-Y",
        description: "Yank the last kill",
        group: "Kill ring",
    },
    ChordDoc {
        keys: "Alt-Y",
        description: "Cycle the kill ring (yank-pop)",
        group: "Kill ring",
    },
    ChordDoc {
        keys: "Ctrl-_ / Ctrl-Z",
        description: "Undo",
        group: "Editing",
    },
    ChordDoc {
        keys: "Shift-Enter / Alt-Enter / Ctrl-J",
        description: "Insert a newline",
        group: "Editing",
    },
    ChordDoc {
        keys: "Enter",
        description: "Submit",
        group: "Editing",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    // -- Test harness --

    /// Builds a bare, focused-agnostic editor for driving directly.
    fn editor() -> TextArea {
        TextArea::new_state()
    }

    /// A plain `char` key press with matching `text`.
    fn char_key(c: char) -> Event {
        Event::KeyPress(Key {
            codepoint: u32::from(c),
            text: Some(c.to_string().into()),
            ..Key::default()
        })
    }

    /// A special-key press by codepoint and modifiers, with no text.
    fn key(codepoint: u32, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint,
            mods,
            ..Key::default()
        })
    }

    /// A modified letter press (e.g. Alt-B), carrying the letter codepoint.
    fn mod_key(c: char, mods: Modifiers) -> Event {
        Event::KeyPress(Key {
            codepoint: u32::from(c),
            mods,
            ..Key::default()
        })
    }

    fn send(ed: &mut TextArea, event: &Event) {
        let mut ctx = EventContext::new();
        ed.handle_event(&mut ctx, event);
    }

    fn type_str(ed: &mut TextArea, s: &str) {
        for c in s.chars() {
            send(ed, &char_key(c));
        }
    }

    fn ctx(w: u16, h: u16) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: crate::vxfw::MaxSize {
                width: Some(w),
                height: Some(h),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        }
    }

    /// Concatenates the grapheme of every cell in `row` of `surf`.
    fn row_text(surf: &Surface, row: u16) -> String {
        let mut out = String::new();
        for col in 0..surf.size.width {
            out.push_str(surf.read_cell(col, row).char.grapheme());
        }
        out
    }

    // -- The required per-module doctest --

    #[test]
    fn text_area() {
        let mut ed = editor();
        type_str(&mut ed, "hi");
        assert_eq!(ed.text(), "hi");
        assert_eq!(ed.cursor(), (0, 2));

        send(&mut ed, &key(Key::BACKSPACE, Modifiers::empty()));
        assert_eq!(ed.text(), "h");

        // Draws a bordered surface: top rule, one content row, bottom rule.
        let surf = ed.draw(&ctx(20, 10));
        assert_eq!(surf.size.height, 3);
        assert!(surf.cursor.is_some());
    }

    // -- Insert / delete --

    #[test]
    fn insert_and_delete_basics() {
        let mut ed = editor();
        type_str(&mut ed, "hello");
        assert_eq!(ed.text(), "hello");
        send(&mut ed, &key(Key::BACKSPACE, Modifiers::empty()));
        assert_eq!(ed.text(), "hell");
        // Ctrl-A to start, delete-forward removes 'h'.
        send(&mut ed, &mod_key('a', Modifiers::CTRL));
        send(&mut ed, &key(Key::DELETE, Modifiers::empty()));
        assert_eq!(ed.text(), "ell");
    }

    #[test]
    fn newline_via_shift_enter() {
        let mut ed = editor();
        type_str(&mut ed, "a");
        send(&mut ed, &key(Key::ENTER, Modifiers::SHIFT));
        type_str(&mut ed, "b");
        assert_eq!(ed.text(), "a\nb");
        assert_eq!(ed.cursor(), (1, 1));
    }

    // -- Word motion (three-class emacs feel) --

    #[test]
    fn word_motion_stops_on_punctuation_runs() {
        // EmacsWords: `...` is its own word between `bar` and `baz`.
        let mut ed = editor();
        ed.set_text("foo bar... baz");
        ed.cursor_col = 0;
        send(&mut ed, &mod_key('f', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 3, "end of foo");
        send(&mut ed, &mod_key('f', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 7, "end of bar");
        send(&mut ed, &mod_key('f', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 10, "end of ...");
        send(&mut ed, &mod_key('f', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 14, "end of baz");
    }

    #[test]
    fn word_motion_left_lands_before_punctuation_run() {
        let mut ed = editor();
        ed.set_text("foo bar...");
        // Cursor at end.
        send(&mut ed, &mod_key('b', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 7, "before ...");
        send(&mut ed, &mod_key('b', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 4, "before bar");
        send(&mut ed, &mod_key('b', Modifiers::ALT));
        assert_eq!(ed.cursor_col, 0, "start");
    }

    #[test]
    fn word_motion_wraps_across_lines() {
        let mut ed = editor();
        ed.set_text("foo\nbar");
        ed.cursor_line = 1;
        ed.cursor_col = 0;
        send(&mut ed, &mod_key('b', Modifiers::ALT));
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 3));
    }

    // -- Vertical movement with sticky column --

    #[test]
    fn vertical_move_sticky_column_over_short_line() {
        // "hello world"(11) / "hi"(2) / "foobar"(6). Down from col 8 clamps to
        // the short line's end while remembering 8, then re-expands.
        let mut ed = editor();
        ed.set_text("hello world\nhi\nfoobar");
        ed.cursor_line = 0;
        ed.cursor_col = 8;

        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 2));
        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (2, 6));
        ed.move_up();
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 2));
        ed.move_up();
        // Sticky column restores column 8 on the long line.
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 8));
    }

    #[test]
    fn vertical_move_preferred_column_survives_middle_line() {
        // A short middle line does not clobber the preferred column.
        let mut ed = editor();
        ed.set_text("abcdefgh\nxy\nabcdefgh");
        ed.cursor_line = 0;
        ed.cursor_col = 5;
        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 2));
        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (2, 5));
    }

    #[test]
    fn vertical_move_target_fits_keeps_column() {
        // Case 1 of the table: starting nav onto a longer line keeps the column.
        let mut ed = editor();
        ed.set_text("abcd\nabcdefgh");
        ed.cursor_line = 0;
        ed.cursor_col = 2;
        ed.move_down();
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 2));
    }

    #[test]
    fn vertical_move_across_wrapped_visual_lines() {
        // "hello world foo" wraps at width 11 into "hello " and "world foo".
        let mut ed = editor();
        ed.set_text("hello world foo");
        ed.layout_width = 11;
        ed.width_method = gwidth::Method::Unicode;
        ed.cursor_line = 0;
        ed.cursor_col = 2;
        ed.move_down();
        // Column 2 of the second visual row is byte 8 ('r' of "world").
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 8));
        ed.move_up();
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 2));
    }

    #[test]
    fn vertical_move_snaps_onto_multibyte_grapheme_and_round_trips() {
        // Moving down from an ASCII byte column that lands strictly inside a
        // multi-byte grapheme snaps the cursor to the segment start and records
        // the pre-snap column, so moving back up restores the original column.
        let mut ed = editor();
        // '中' occupies bytes [1, 4); byte column 3 is strictly inside it.
        ed.set_text("abcd\nx中y");
        ed.cursor_line = 0;
        ed.cursor_col = 3;

        ed.move_down();
        // Snapped to the start of '中' (byte 1), not left mid-grapheme.
        assert_eq!((ed.cursor_line, ed.cursor_col), (1, 1));
        assert_eq!(ed.snapped_from_cursor_col, Some(3));

        ed.move_up();
        // The pre-snap anchor restores byte column 3 on the ASCII line.
        assert_eq!((ed.cursor_line, ed.cursor_col), (0, 3));
        assert_eq!(ed.snapped_from_cursor_col, None);
    }

    // -- Wrapping --

    #[test]
    fn wrap_reconstructs_line_exactly() {
        let ed = editor();
        let line = "some long text with several words to wrap";
        let spans = ed.wrap_line_spans(line, 10);
        let joined: String = spans.iter().map(|&(s, e)| &line[s..e]).collect();
        assert_eq!(joined, line);
        for &(s, e) in &spans {
            assert!(s <= e && e <= line.len());
        }
    }

    #[test]
    fn wrap_breaks_at_word_opportunity() {
        let ed = editor();
        let spans = ed.wrap_line_spans("hello world foo", 11);
        assert_eq!(spans, vec![(0, 6), (6, 15)]);
    }

    #[test]
    fn wrap_force_breaks_a_long_word() {
        let ed = editor();
        // No whitespace, so every overflow force-breaks at the width.
        let spans = ed.wrap_line_spans("abcdefghijklmnop", 5);
        assert_eq!(spans, vec![(0, 5), (5, 10), (10, 15), (15, 16)]);
    }

    #[test]
    fn wrap_keeps_oversized_grapheme_whole() {
        let ed = editor();
        // Each CJK char is width 2 and 3 bytes; at width 1 each stays whole.
        let line = "中中";
        let spans = ed.wrap_line_spans(line, 1);
        assert_eq!(spans, vec![(0, 3), (3, 6)]);
        let joined: String = spans.iter().map(|&(s, e)| &line[s..e]).collect();
        assert_eq!(joined, line);
    }

    #[test]
    fn wrap_reconstructs_multi_and_trailing_spaces() {
        // Runs of spaces and a trailing space must survive wrapping so the
        // visual-line map stays byte-synced with the document navigation reads.
        let ed = editor();
        let line = "a  bb   c  ";
        for width in 1..=12 {
            let spans = ed.wrap_line_spans(line, width);
            let joined: String = spans.iter().map(|&(s, e)| &line[s..e]).collect();
            assert_eq!(joined, line, "width {width} must reconstruct the line");
            let mut prev_end = 0;
            for &(s, e) in &spans {
                assert_eq!(s, prev_end, "spans must be contiguous at width {width}");
                assert!(s <= e && e <= line.len());
                prev_end = e;
            }
            assert_eq!(prev_end, line.len(), "spans must cover the line");
        }
    }

    // -- Kill ring --

    #[test]
    fn kill_to_end_and_yank() {
        let mut ed = editor();
        ed.set_text("hello world");
        ed.cursor_col = 5;
        send(&mut ed, &mod_key('k', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello");
        assert_eq!(ed.kill_ring.peek(), Some(" world"));
        send(&mut ed, &mod_key('y', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello world");
    }

    #[test]
    fn kill_word_backward_accumulates_with_prepend() {
        let mut ed = editor();
        ed.set_text("ab cd");
        // Cursor at end. Two backward word kills accumulate in prepend order.
        send(&mut ed, &mod_key('w', Modifiers::CTRL));
        assert_eq!(ed.text(), "ab ");
        assert_eq!(ed.kill_ring.peek(), Some("cd"));
        send(&mut ed, &mod_key('w', Modifiers::CTRL));
        assert_eq!(ed.text(), "");
        assert_eq!(ed.kill_ring.peek(), Some("ab cd"));
    }

    #[test]
    fn yank_pop_rotates_the_ring() {
        let mut ed = editor();
        ed.set_text("hello world");
        ed.cursor_col = 5;
        // First kill: " world".
        ed.kill_to_end();
        // Break the kill chain, then kill "hello" as a separate entry.
        ed.last_action = LastAction::None;
        ed.cursor_col = 0;
        ed.kill_to_end();
        assert_eq!(ed.text(), "");
        // Yank the most recent ("hello"), then yank-pop to the older (" world").
        send(&mut ed, &mod_key('y', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello");
        send(&mut ed, &mod_key('y', Modifiers::ALT));
        assert_eq!(ed.text(), " world");
    }

    // -- Undo coalescing --

    #[test]
    fn undo_coalesces_words_not_whitespace() {
        // Typing "hello world" pushes two snapshots: before 'h' and before ' '.
        let mut ed = editor();
        type_str(&mut ed, "hello world");
        assert_eq!(ed.text(), "hello world");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "");
    }

    #[test]
    fn undo_each_space_separately() {
        // "hello  " (two spaces) pushes three snapshots.
        let mut ed = editor();
        type_str(&mut ed, "hello  ");
        assert_eq!(ed.text(), "hello  ");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello ");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "hello");
        send(&mut ed, &mod_key('z', Modifiers::CTRL));
        assert_eq!(ed.text(), "");
    }

    // -- History --

    #[test]
    fn history_draft_preserved_and_restored() {
        // The history_up method parks the in-progress buffer as a draft and
        // history_down restores it.
        let mut ed = editor();
        ed.add_to_history("first");
        ed.add_to_history("second");
        ed.set_document("draft");

        ed.history_up();
        assert_eq!(ed.text(), "second");
        ed.history_up();
        assert_eq!(ed.text(), "first");
        ed.history_down();
        assert_eq!(ed.text(), "second");
        ed.history_down();
        assert_eq!(ed.text(), "draft");
    }

    #[test]
    fn history_up_on_empty_prompt_walks_newest_first() {
        let mut ed = editor();
        ed.seed_history(&[
            "oldest".to_string(),
            "middle".to_string(),
            "newest".to_string(),
        ]);
        send(&mut ed, &key(Key::UP, Modifiers::empty()));
        assert_eq!(ed.text(), "newest");
        send(&mut ed, &key(Key::UP, Modifiers::empty()));
        assert_eq!(ed.text(), "middle");
        send(&mut ed, &key(Key::UP, Modifiers::empty()));
        assert_eq!(ed.text(), "oldest");
    }

    #[test]
    fn up_on_nonempty_top_line_jumps_to_start_not_history() {
        // With typed content, Up moves the cursor rather than entering history.
        let mut ed = editor();
        ed.add_to_history("prev");
        type_str(&mut ed, "typed");
        send(&mut ed, &key(Key::UP, Modifiers::empty()));
        assert_eq!(ed.text(), "typed");
        assert_eq!(ed.cursor(), (0, 0));
    }

    // -- Submit / newline / backslash --

    #[test]
    fn submit_fires_callback_and_clears() {
        let seen = Rc::new(RefCell::new(String::new()));
        let mut ed = editor();
        let sink = Rc::clone(&seen);
        ed.on_submit = Some(Box::new(move |_ctx, s| {
            *sink.borrow_mut() = s.to_string();
        }));
        type_str(&mut ed, "  hi there  ");
        send(&mut ed, &key(Key::ENTER, Modifiers::empty()));
        assert_eq!(seen.borrow().as_str(), "hi there");
        assert_eq!(ed.text(), "");
        assert_eq!(ed.take_submitted().as_deref(), Some("hi there"));
    }

    #[test]
    fn submit_disabled_is_silent() {
        let mut ed = editor();
        ed.set_submit_enabled(false);
        type_str(&mut ed, "keep");
        send(&mut ed, &key(Key::ENTER, Modifiers::empty()));
        assert_eq!(ed.text(), "keep");
    }

    #[test]
    fn backslash_enter_inserts_newline() {
        let mut ed = editor();
        type_str(&mut ed, "a\\");
        send(&mut ed, &key(Key::ENTER, Modifiers::empty()));
        assert_eq!(ed.text(), "a\n");
        assert_eq!(ed.take_submitted(), None);
    }

    // -- Palette trigger --

    #[test]
    fn palette_trigger_fires_on_slash_at_empty_start() {
        let fired = Rc::new(RefCell::new(false));
        let mut ed = editor();
        let flag = Rc::clone(&fired);
        ed.on_palette_trigger = Some(Box::new(move |_ctx| {
            *flag.borrow_mut() = true;
        }));
        send(&mut ed, &char_key('/'));
        assert!(*fired.borrow());
        assert_eq!(ed.text(), "");
    }

    #[test]
    fn slash_inserts_without_palette_trigger() {
        let mut ed = editor();
        send(&mut ed, &char_key('/'));
        assert_eq!(ed.text(), "/");
    }

    #[test]
    fn palette_trigger_ignores_mid_line_slash() {
        let fired = Rc::new(RefCell::new(false));
        let mut ed = editor();
        let flag = Rc::clone(&fired);
        ed.on_palette_trigger = Some(Box::new(move |_ctx| {
            *flag.borrow_mut() = true;
        }));
        type_str(&mut ed, "ab");
        send(&mut ed, &char_key('/'));
        assert!(!*fired.borrow());
        assert_eq!(ed.text(), "ab/");
    }

    // -- on_change --

    #[test]
    fn on_change_fires_only_on_actual_edits() {
        let seen = Rc::new(RefCell::new(String::new()));
        let mut ed = editor();
        let sink = Rc::clone(&seen);
        ed.on_change = Some(Box::new(move |_ctx, s| {
            *sink.borrow_mut() = s.to_string();
        }));
        type_str(&mut ed, "hi");
        assert_eq!(seen.borrow().as_str(), "hi");
        // A cursor-only move does not change the text.
        *seen.borrow_mut() = "sentinel".to_string();
        send(&mut ed, &mod_key('a', Modifiers::CTRL));
        assert_eq!(seen.borrow().as_str(), "sentinel");
    }

    // -- Rendering --

    #[test]
    fn top_bar_label_inlaid_in_top_rule() {
        let mut ed = editor();
        ed.set_top_bar_label(Some("agent 2".to_string()));
        let surf = ed.draw(&ctx(40, 10));
        assert!(row_text(&surf, 0).contains("agent 2"));
    }

    #[test]
    fn no_top_bar_label_is_a_plain_rule() {
        let mut ed = editor();
        let surf = ed.draw(&ctx(20, 10));
        assert_eq!(row_text(&surf, 0), "─".repeat(20));
    }

    #[test]
    fn scroll_indicator_appears_when_scrolled() {
        let mut ed = editor();
        ed.set_max_visible_rows(Some(2));
        ed.set_text("l1\nl2\nl3\nl4\nl5");
        let surf = ed.draw(&ctx(40, 10));
        let top = row_text(&surf, 0);
        assert!(top.contains('↑'), "top rule: {top:?}");
        assert!(top.contains("more"), "top rule: {top:?}");
        // Two content rows plus two rules.
        assert_eq!(surf.size.height, 4);
    }

    #[test]
    fn cursor_is_reported_at_expected_cell() {
        let mut ed = editor();
        type_str(&mut ed, "abc");
        let surf = ed.draw(&ctx(20, 10));
        let cursor = surf.cursor.expect("cursor reported");
        // Row 1 (below the top rule), column 3 (after "abc"), no padding.
        assert_eq!((cursor.row, cursor.col), (1, 3));
    }

    #[test]
    fn padding_shifts_content_and_cursor() {
        let mut ed = editor();
        ed.set_padding_x(2);
        type_str(&mut ed, "x");
        let surf = ed.draw(&ctx(20, 10));
        // Content starts at the padding column.
        assert_eq!(surf.read_cell(2, 1).char.grapheme(), "x");
        let cursor = surf.cursor.expect("cursor reported");
        assert_eq!(cursor.col, 3);
    }

    // -- bindings table --

    #[test]
    fn bindings_cover_the_core_chords() {
        let bindings = TextArea::bindings();
        assert!(!bindings.is_empty());
        assert!(bindings.iter().any(|b| b.description.contains("Submit")));
        assert!(bindings.iter().any(|b| b.description.contains("Undo")));
        assert!(bindings.iter().any(|b| b.keys.contains("Ctrl-Y")));
    }
}
