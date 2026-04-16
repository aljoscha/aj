//! Single-line text input component with Emacs keybindings.

use crossterm::event::{KeyCode, KeyModifiers};
use unicode_segmentation::UnicodeSegmentation;

use crate::ansi::visible_width;
use crate::component::{CURSOR_MARKER, Component};
use crate::keybindings;
use crate::keys::{InputEvent, is_newline_event};
use crate::kill_ring::KillRing;
use crate::undo_stack::UndoStack;

/// Snapshot saved per undo entry: the text value and the byte-offset
/// cursor position at the time the snapshot was taken.
type InputSnapshot = (String, usize);

/// A single-line text input with Emacs keybindings, kill ring, and undo.
pub struct Input {
    value: String,
    cursor: usize, // Byte offset into value.
    prompt: String,
    focused: bool,
    scroll_offset: usize,
    kill_ring: KillRing,
    undo_stack: UndoStack<InputSnapshot>,
    last_action: LastAction,
    /// Length in bytes of the text inserted by the last yank operation,
    /// so yank-pop (Alt+Y) knows how many bytes to remove before
    /// inserting the next ring entry. Only valid when `last_action` is
    /// `LastAction::Yank`.
    last_yank_len: usize,

    /// Called when the user presses Enter.
    pub on_submit: Option<Box<dyn FnMut(&str)>>,
    /// Called when the user presses Escape.
    pub on_escape: Option<Box<dyn FnMut()>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastAction {
    None,
    Kill,
    Yank,
    TypeWord,
}

impl Input {
    pub fn new(prompt: &str) -> Self {
        Self {
            value: String::new(),
            cursor: 0,
            prompt: prompt.to_string(),
            focused: false,
            scroll_offset: 0,
            kill_ring: KillRing::default(),
            undo_stack: UndoStack::default(),
            last_action: LastAction::None,
            last_yank_len: 0,

            on_submit: None,
            on_escape: None,
        }
    }

    /// Get the current value.
    pub fn value(&self) -> &str {
        &self.value
    }

    /// Replace the input's value, keeping the existing cursor position
    /// where it still fits in the new string.
    ///
    /// Mirrors pi-tui's `Input.setValue` (`this.cursor = Math.min(
    /// this.cursor, value.length)`), so a programmatic value swap
    /// during an in-flight edit doesn't yank the caret to the end and
    /// disrupt the user's flow. If the previous cursor was past the
    /// new length, it clamps to the end of the new value.
    ///
    /// The clamped offset is snapped *forward* to the next char
    /// boundary as a defensive guard: pi works in JS code units (no
    /// panic on mid-codepoint indices), but our Rust port byte-slices
    /// `value` elsewhere (`&self.value[..self.cursor]`) and would
    /// panic on a mid-multi-byte cursor. Forward (rather than
    /// backward) mirrors pi's effective semantic that the cursor is
    /// "past" the partial codepoint after the new value's UTF-8
    /// layout shifts under it; in pi's UTF-16 parallel, the analogous
    /// minimum that yielded a code-unit boundary would land at the
    /// position after the disrupted character. The snap is a no-op on
    /// the common case where `cursor` was already on a grapheme
    /// boundary in the old value and stays in-range for the new one.
    pub fn set_value(&mut self, value: &str) {
        self.value = value.to_string();
        self.cursor = self.cursor.min(self.value.len());
        while self.cursor < self.value.len() && !self.value.is_char_boundary(self.cursor) {
            self.cursor += 1;
        }
    }

    /// Get the cursor position (byte offset).
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Clear the input.
    pub fn clear(&mut self) {
        self.undo_stack.push((self.value.clone(), self.cursor));
        self.value.clear();
        self.cursor = 0;
        self.scroll_offset = 0;
        self.last_action = LastAction::None;
    }

    // -- Grapheme helpers --

    /// Get grapheme boundaries as byte offsets.
    fn grapheme_boundaries(&self) -> Vec<usize> {
        let mut boundaries = vec![0];
        for (i, _) in self.value.grapheme_indices(true) {
            if i > 0 {
                boundaries.push(i);
            }
        }
        boundaries.push(self.value.len());
        boundaries
    }

    /// Move cursor left by one grapheme.
    fn move_left(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let bounds = self.grapheme_boundaries();
        for i in (0..bounds.len()).rev() {
            if bounds[i] < self.cursor {
                self.cursor = bounds[i];
                return;
            }
        }
    }

    /// Move cursor right by one grapheme.
    fn move_right(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        let bounds = self.grapheme_boundaries();
        for &b in &bounds {
            if b > self.cursor {
                self.cursor = b;
                return;
            }
        }
    }

    /// Find the byte offset of the previous word boundary.
    ///
    /// Thin wrapper over [`crate::word_boundary::word_boundary_left`]
    /// — see that function for the three-class segmentation model
    /// (whitespace / punctuation / word) shared with the multi-line
    /// `Editor` component.
    fn word_boundary_left(&self) -> usize {
        crate::word_boundary::word_boundary_left(&self.value, self.cursor)
    }

    /// Find the byte offset of the next word boundary.
    ///
    /// Thin wrapper over [`crate::word_boundary::word_boundary_right`].
    fn word_boundary_right(&self) -> usize {
        crate::word_boundary::word_boundary_right(&self.value, self.cursor)
    }

    /// Insert a character at the cursor position.
    fn insert_char(&mut self, c: char) {
        // Undo coalescing rule (matches pi-tui's `Input.insertCharacter`
        // and our own `Editor::insert_char`):
        //
        // - Consecutive non-whitespace characters merge into one undo
        //   unit. Typing "hello" pushes exactly one snapshot (before
        //   the first char).
        // - Every whitespace character pushes its own snapshot, so each
        //   space is separately undoable. Typing "hello  " pushes
        //   three: before 'h', before the first ' ', before the
        //   second ' '.
        // - Critically, the next word after whitespace does NOT push a
        //   new snapshot — `last_action` is set to `TypeWord` even
        //   after a whitespace insert, so the following character
        //   reads as a continuation. Typing "hello world" pushes two
        //   snapshots (before 'h', before ' '), and two undos take it
        //   back to empty (one removes " world", the next removes
        //   "hello").
        //
        // The previous form ran a 2x4 match table with a separate
        // `TypeWhitespace` state. Observable behavior was identical in
        // every test scenario — the (false, TypeWhitespace) → no-push
        // arm produced the same outcome as pi-tui's "always TypeWord,
        // never push when continuing" rule — so we collapsed to the
        // upstream shape for parity and to mirror the editor.
        if c.is_whitespace() || self.last_action != LastAction::TypeWord {
            self.undo_stack.push((self.value.clone(), self.cursor));
        }
        self.value.insert(self.cursor, c);
        self.cursor += c.len_utf8();
        // Always `TypeWord` after a character insert, including for
        // whitespace — this is what prevents the next word from pushing
        // its own snapshot.
        self.last_action = LastAction::TypeWord;
    }

    /// Delete the grapheme before the cursor.
    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.undo_stack.push((self.value.clone(), self.cursor));
        let old_cursor = self.cursor;
        self.move_left();
        self.value.drain(self.cursor..old_cursor);
        self.last_action = LastAction::None;
    }

    /// Delete the grapheme after the cursor.
    fn delete_forward(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.undo_stack.push((self.value.clone(), self.cursor));
        let bounds = self.grapheme_boundaries();
        let next = bounds
            .iter()
            .find(|&&b| b > self.cursor)
            .copied()
            .unwrap_or(self.value.len());
        self.value.drain(self.cursor..next);
        self.last_action = LastAction::None;
    }

    /// Delete word backward, adding to kill ring.
    fn kill_word_backward(&mut self) {
        let target = self.word_boundary_left();
        if target == self.cursor {
            return;
        }
        self.undo_stack.push((self.value.clone(), self.cursor));
        let deleted: String = self.value.drain(target..self.cursor).collect();
        self.kill_ring
            .push(&deleted, true, self.last_action == LastAction::Kill);
        self.cursor = target;
        self.last_action = LastAction::Kill;
    }

    /// Delete word forward, adding to kill ring.
    fn kill_word_forward(&mut self) {
        let target = self.word_boundary_right();
        if target == self.cursor {
            return;
        }
        self.undo_stack.push((self.value.clone(), self.cursor));
        let deleted: String = self.value.drain(self.cursor..target).collect();
        self.kill_ring
            .push(&deleted, false, self.last_action == LastAction::Kill);
        self.last_action = LastAction::Kill;
    }

    /// Kill from cursor to end of line.
    fn kill_to_end(&mut self) {
        if self.cursor >= self.value.len() {
            return;
        }
        self.undo_stack.push((self.value.clone(), self.cursor));
        let deleted: String = self.value.drain(self.cursor..).collect();
        self.kill_ring
            .push(&deleted, false, self.last_action == LastAction::Kill);
        self.last_action = LastAction::Kill;
    }

    /// Kill from cursor to start of line.
    fn kill_to_start(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.undo_stack.push((self.value.clone(), self.cursor));
        let deleted: String = self.value.drain(..self.cursor).collect();
        self.kill_ring
            .push(&deleted, true, self.last_action == LastAction::Kill);
        self.cursor = 0;
        self.last_action = LastAction::Kill;
    }

    /// Yank the most recent kill ring entry.
    fn yank(&mut self) {
        if let Some(text) = self.kill_ring.peek().map(|s| s.to_string()) {
            self.undo_stack.push((self.value.clone(), self.cursor));
            self.value.insert_str(self.cursor, &text);
            self.cursor += text.len();
            self.last_action = LastAction::Yank;
            self.last_yank_len = text.len();
        }
    }

    /// Yank-pop: rotate the kill ring and replace the last-yanked text
    /// with the newly-surfaced entry. No-op if the last action was not
    /// a yank (the rotation only makes sense as a continuation of a
    /// yank).
    fn yank_pop(&mut self) {
        if self.last_action != LastAction::Yank {
            return;
        }
        if self.kill_ring.len() < 2 {
            return;
        }
        // Push an undo snapshot before the rotation so undo lands on
        // the previously-yanked content (the state that was visible
        // just before this yank-pop fired) rather than the pre-yank
        // state. Mirrors pi-tui's `yankPop` shape (`pushUndo()` at
        // entry, `lastAction = "yank"` at exit). Each yank-pop in a
        // chain becomes its own undo step.
        self.undo_stack.push((self.value.clone(), self.cursor));
        // Remove the text the previous yank inserted, rotate, insert
        // the new top entry.
        let start = self.cursor - self.last_yank_len;
        self.value.drain(start..self.cursor);
        self.cursor = start;
        self.kill_ring.rotate();
        if let Some(text) = self.kill_ring.peek().map(|s| s.to_string()) {
            self.value.insert_str(self.cursor, &text);
            self.cursor += text.len();
            self.last_yank_len = text.len();
        }
        // Reassert the yank state so a follow-up Alt+Y still rotates.
        // Defensive: nothing in the body currently mutates
        // `last_action`, but pi-tui's structural invariant is that
        // yank-pop ends in the "yank" state, and the explicit set
        // protects against future drift in the rotate/insert path.
        self.last_action = LastAction::Yank;
    }

    /// Undo the last action.
    fn undo(&mut self) {
        if let Some((val, cur)) = self.undo_stack.pop() {
            self.value = val;
            self.cursor = cur;
            self.last_action = LastAction::None;
        }
    }
}

impl Component for Input {
    crate::impl_component_any!();

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }

    fn render(&mut self, width: usize) -> Vec<String> {
        let prompt_width = visible_width(&self.prompt);
        let available = width.saturating_sub(prompt_width);
        if available == 0 {
            return vec![self.prompt.clone()];
        }

        // Calculate visible portion of the value with horizontal scrolling.
        let value_graphemes: Vec<&str> = self.value.graphemes(true).collect();

        // Find cursor position in grapheme indices.
        let mut cursor_grapheme_idx = 0;
        let mut byte_pos = 0;
        for (i, g) in value_graphemes.iter().enumerate() {
            if byte_pos >= self.cursor {
                cursor_grapheme_idx = i;
                break;
            }
            byte_pos += g.len();
            if byte_pos >= self.cursor {
                cursor_grapheme_idx = i + 1;
                break;
            }
        }
        if byte_pos < self.cursor {
            cursor_grapheme_idx = value_graphemes.len();
        }

        // Determine scroll offset to center cursor in available space.
        let half = available / 2;
        let scroll = if cursor_grapheme_idx > half {
            cursor_grapheme_idx - half
        } else {
            0
        };

        // Build visible string.
        let mut visible = String::new();
        let mut vis_width = 0;
        for (i, g) in value_graphemes.iter().enumerate() {
            if i < scroll {
                continue;
            }
            let gw = visible_width(g);
            if vis_width + gw > available {
                break;
            }
            visible.push_str(g);
            vis_width += gw;
        }

        // Build the output line with cursor.
        let mut line = self.prompt.clone();

        if self.focused {
            // Insert cursor marker and reverse-video cursor character.
            let pre_cursor: String = self
                .value
                .graphemes(true)
                .skip(scroll)
                .take(cursor_grapheme_idx - scroll)
                .collect();
            let cursor_char: Option<&str> = value_graphemes.get(cursor_grapheme_idx).copied();
            let post_cursor: String = self
                .value
                .graphemes(true)
                .skip(scroll)
                .skip(cursor_grapheme_idx - scroll + if cursor_char.is_some() { 1 } else { 0 })
                .take_while({
                    let mut w =
                        visible_width(&pre_cursor) + cursor_char.map(visible_width).unwrap_or(1);
                    move |g| {
                        let gw = visible_width(*g);
                        if w + gw <= available {
                            w += gw;
                            true
                        } else {
                            false
                        }
                    }
                })
                .collect();

            line.push_str(&pre_cursor);
            line.push_str(CURSOR_MARKER);
            match cursor_char {
                Some(c) => {
                    line.push_str(&format!("\x1b[7m{}\x1b[27m", c));
                }
                None => {
                    line.push_str("\x1b[7m \x1b[27m");
                }
            }
            line.push_str(&post_cursor);
        } else {
            line.push_str(&visible);
        }

        vec![line]
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        match event {
            InputEvent::Key(_) => {
                let kb = keybindings::get();

                // Cancel: matches `tui.select.cancel` (Escape or Ctrl+C
                // by default). Fires `on_escape` for both — the
                // original framework's Input also treats Ctrl+C as
                // "user wants out" rather than letting it bubble to the
                // parent. Application code that wants explicit Ctrl+C
                // handling at the top level can route it through
                // `on_escape`.
                if kb.matches(event, "tui.select.cancel") {
                    if let Some(ref mut on_escape) = self.on_escape {
                        on_escape();
                    }
                    return true;
                }

                if kb.matches(event, "tui.editor.undo") {
                    self.undo();
                    return true;
                }

                // Submit: matches `tui.input.submit` (plain Enter by
                // default), plus the byte-form fallbacks
                // [`is_newline_event`] catches.
                //
                // Pi-tui's `input.ts:100` uses the narrower fallback
                // `data === "\n"` (literal LF byte). Mapped through
                // crossterm's parsing, that single byte arrives as
                // either `KeyCode::Char('\n')` (non-raw mode) or
                // `KeyCode::Char('j') + CTRL` (raw mode — Ctrl+J is
                // ASCII LF, 0x0A). `is_newline_event` recognizes both
                // of those *and* one extra form pi-tui's Input does
                // not submit on: `KeyCode::Enter + ALT` (the Alt+Enter
                // byte sequence `\x1b\r`). Pi-tui's Input silently
                // drops Alt+Enter at the printable-character tail
                // (the string contains `\x1b`, which fails the
                // control-char filter).
                //
                // We deliberately accept the broader set here, for
                // three reasons:
                // 1. The Editor's newline branch (F17) already uses
                //    the same helper — keeping the recognizer shared
                //    means byte-form handling is symmetric across the
                //    two text components.
                // 2. Submitting on Alt+Enter is strictly better UX
                //    than silently swallowing it (pi's behavior is
                //    arguably a bug-of-omission rather than a
                //    deliberate design choice).
                // 3. Plain Enter, Shift+Enter, Ctrl+Enter, and other
                //    modified Enter events are intentionally excluded
                //    from `is_newline_event` so the registry can
                //    route user-rebound submit / newLine bindings
                //    without interference.
                //
                // Documented in `PORTING.md` (F20) as a deliberate
                // divergence from pi.
                if kb.matches(event, "tui.input.submit") || is_newline_event(event) {
                    if let Some(ref mut on_submit) = self.on_submit {
                        on_submit(&self.value);
                    }
                    self.undo_stack.clear();
                    return true;
                }

                if kb.matches(event, "tui.editor.deleteCharBackward") {
                    self.backspace();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteCharForward") {
                    self.delete_forward();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteWordBackward") {
                    self.kill_word_backward();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteWordForward") {
                    self.kill_word_forward();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteToLineStart") {
                    self.kill_to_start();
                    return true;
                }
                if kb.matches(event, "tui.editor.deleteToLineEnd") {
                    self.kill_to_end();
                    return true;
                }

                if kb.matches(event, "tui.editor.yank") {
                    self.yank();
                    return true;
                }
                if kb.matches(event, "tui.editor.yankPop") {
                    self.yank_pop();
                    return true;
                }

                if kb.matches(event, "tui.editor.cursorLeft") {
                    self.move_left();
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorRight") {
                    self.move_right();
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorLineStart") {
                    self.cursor = 0;
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorLineEnd") {
                    self.cursor = self.value.len();
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorWordLeft") {
                    self.cursor = self.word_boundary_left();
                    self.last_action = LastAction::None;
                    return true;
                }
                if kb.matches(event, "tui.editor.cursorWordRight") {
                    self.cursor = self.word_boundary_right();
                    self.last_action = LastAction::None;
                    return true;
                }

                // Drop the read guard before falling through to
                // character insertion: that path doesn't consult the
                // registry, and holding the guard across the rest of
                // the handler is unnecessary lock pressure.
                drop(kb);

                // Printable characters (no Ctrl/Alt; Shift folded into
                // case by the terminal). Mirrors the original
                // framework's "accept printable, reject control" tail.
                if let InputEvent::Key(key) = event {
                    if let KeyCode::Char(c) = key.code {
                        if (key.modifiers - KeyModifiers::SHIFT).is_empty() {
                            self.insert_char(c);
                            return true;
                        }
                    }
                }
                false
            }
            InputEvent::Paste(text) => {
                self.undo_stack.push((self.value.clone(), self.cursor));
                // Strip newlines and control chars from paste. Tabs
                // expand to four spaces so a pasted indent reads
                // correctly in a single-line context (matches the
                // Editor's paste-tab handling).
                let mut cleaned = String::with_capacity(text.len());
                for c in text.chars() {
                    if c == '\t' {
                        cleaned.push_str("    ");
                    } else if !c.is_control() {
                        cleaned.push(c);
                    }
                }
                self.value.insert_str(self.cursor, &cleaned);
                self.cursor += cleaned.len();
                self.last_action = LastAction::None;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_input_basic() {
        let mut input = Input::new("> ");
        input.set_focused(true);
        let lines = input.render(80);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].starts_with("> "));
    }

    #[test]
    fn test_input_typing() {
        let mut input = Input::new("> ");
        input.handle_input(&InputEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('h'),
            KeyModifiers::NONE,
        )));
        assert_eq!(input.value(), "h");
        input.handle_input(&InputEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('i'),
            KeyModifiers::NONE,
        )));
        assert_eq!(input.value(), "hi");
    }

    #[test]
    fn test_input_backspace() {
        let mut input = Input::new("> ");
        input.set_value("hello");
        // F21: set_value clamps the existing cursor (= 0 for a fresh
        // input) rather than moving it to end. This test wants
        // backspace to delete the trailing 'o', so seed the cursor
        // explicitly at the end of the value.
        input.cursor = input.value.len();
        input.handle_input(&InputEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
        assert_eq!(input.value(), "hell");
    }

    #[test]
    fn test_input_kill_ring() {
        let mut input = Input::new("> ");
        input.set_value("hello world");
        // Ctrl+K: kill to end of line.
        input.cursor = 5;
        input.handle_input(&InputEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('k'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(input.value(), "hello");
        // Ctrl+Y: yank.
        input.handle_input(&InputEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(input.value(), "hello world");
    }

    #[test]
    fn test_input_undo() {
        let mut input = Input::new("> ");
        input.set_value("hello");
        // F21: see `test_input_backspace` — seed cursor at end so the
        // backspace below targets the trailing 'o'.
        input.cursor = input.value.len();
        let _original_cursor = input.cursor;
        // Delete a character.
        input.handle_input(&InputEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Backspace,
            KeyModifiers::NONE,
        )));
        assert_eq!(input.value(), "hell");
        // Undo.
        input.handle_input(&InputEvent::Key(crossterm::event::KeyEvent::new(
            KeyCode::Char('-'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(input.value(), "hello");
    }

    #[test]
    fn test_input_paste() {
        let mut input = Input::new("> ");
        input.handle_input(&InputEvent::Paste("pasted text".to_string()));
        assert_eq!(input.value(), "pasted text");
    }
}
