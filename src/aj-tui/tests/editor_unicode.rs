//! Tests for Editor Unicode text-editing behavior.
//!
//! These cover typing, backspace, and cursor movement over non-ASCII
//! input — specifically umlauts (multi-byte but single grapheme) and
//! emojis (multiple scalar values forming one grapheme cluster). The
//! editor must treat each grapheme as an atomic unit: arrow keys move
//! across whole graphemes, Backspace deletes whole graphemes, and
//! insertion at a mid-string cursor lands between graphemes, never
//! inside one.
//!
//! The word-navigation cases here exercise the three-class segmentation
//! model (whitespace / punctuation / word) and cross-line wrapping —
//! see `Editor::word_boundary_left` in the source for the classifier.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::Key;
use aj_tui::tui::RenderHandle;

fn editor() -> Editor {
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e
}

// ---------------------------------------------------------------------------
// Insertion
// ---------------------------------------------------------------------------

#[test]
fn inserts_mixed_ascii_umlauts_and_emojis_as_literal_text() {
    let mut e = editor();
    for c in ['H', 'e', 'l', 'l', 'o', ' ', 'ä', 'ö', 'ü', ' ', '😀'] {
        e.handle_input(&Key::char(c));
    }
    assert_eq!(e.get_text(), "Hello äöü 😀");
}

// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------

#[test]
fn backspace_deletes_one_multibyte_umlaut_per_press() {
    let mut e = editor();
    for c in ['ä', 'ö', 'ü'] {
        e.handle_input(&Key::char(c));
    }
    e.handle_input(&Key::backspace());
    assert_eq!(e.get_text(), "äö");
}

#[test]
fn backspace_deletes_one_emoji_grapheme_cluster_per_press() {
    let mut e = editor();
    e.handle_input(&Key::char('😀'));
    e.handle_input(&Key::char('👍'));
    e.handle_input(&Key::backspace());
    // The whole 👍 grapheme is removed — not a partial scalar value.
    assert_eq!(e.get_text(), "😀");
}

// ---------------------------------------------------------------------------
// Cursor movement across graphemes
// ---------------------------------------------------------------------------

#[test]
fn insertion_lands_between_umlauts_after_cursor_movement() {
    let mut e = editor();
    for c in ['ä', 'ö', 'ü'] {
        e.handle_input(&Key::char(c));
    }
    // Two Lefts jump over 'ü' and 'ö', landing between 'ä' and 'ö'.
    e.handle_input(&Key::left());
    e.handle_input(&Key::left());
    e.handle_input(&Key::char('x'));
    assert_eq!(e.get_text(), "äxöü");
}

#[test]
fn left_arrow_jumps_over_multi_scalar_emojis_as_single_graphemes() {
    let mut e = editor();
    for c in ['😀', '👍', '🎉'] {
        e.handle_input(&Key::char(c));
    }
    // Each Left moves past a whole emoji; two Lefts land between
    // 😀 and 👍.
    e.handle_input(&Key::left());
    e.handle_input(&Key::left());
    e.handle_input(&Key::char('x'));
    assert_eq!(e.get_text(), "😀x👍🎉");
}

// ---------------------------------------------------------------------------
// Line breaks and set_text
// ---------------------------------------------------------------------------

#[test]
fn newline_preserves_umlauts_on_both_sides_of_the_break() {
    let mut e = editor();
    for c in ['ä', 'ö', 'ü'] {
        e.handle_input(&Key::char(c));
    }
    // Shift+Enter inserts a newline without submitting.
    e.handle_input(&Key::shift_enter());
    for c in ['Ä', 'Ö', 'Ü'] {
        e.handle_input(&Key::char(c));
    }
    assert_eq!(e.get_text(), "äöü\nÄÖÜ");
}

#[test]
fn set_text_accepts_full_unicode_payload_verbatim() {
    let mut e = editor();
    e.set_text("Hällö Wörld! 😀 äöüÄÖÜß");
    assert_eq!(e.get_text(), "Hällö Wörld! 😀 äöüÄÖÜß");
}

// ---------------------------------------------------------------------------
// Ctrl+A (line start) + insertion
// ---------------------------------------------------------------------------

#[test]
fn ctrl_a_moves_cursor_to_line_start_and_next_insert_lands_there() {
    // "Line start" on a single-line buffer is indistinguishable from
    // "document start" — this test covers only the single-line case.
    // Multi-line Ctrl+A semantics will land with the dedicated
    // navigation slice.
    let mut e = editor();
    e.handle_input(&Key::char('a'));
    e.handle_input(&Key::char('b'));
    e.handle_input(&Key::ctrl('a'));
    e.handle_input(&Key::char('x'));
    assert_eq!(e.get_text(), "xab");
}

// ---------------------------------------------------------------------------
// Word deletion: Ctrl+W / Alt+Backspace
// ---------------------------------------------------------------------------

fn ctrl_w(e: &mut Editor) {
    e.handle_input(&Key::ctrl('w'));
}

#[test]
fn ctrl_w_deletes_the_last_space_separated_word() {
    let mut e = editor();
    e.set_text("foo bar baz");
    ctrl_w(&mut e);
    assert_eq!(e.get_text(), "foo bar ");
}

#[test]
fn ctrl_w_eats_trailing_whitespace_then_the_preceding_word() {
    let mut e = editor();
    e.set_text("foo bar   ");
    ctrl_w(&mut e);
    assert_eq!(e.get_text(), "foo ");
}

#[test]
fn ctrl_w_treats_punctuation_run_as_its_own_word() {
    let mut e = editor();
    e.set_text("foo bar...");
    ctrl_w(&mut e);
    // The `...` punctuation run is killed first; the word "bar"
    // survives to the next Ctrl+W.
    assert_eq!(e.get_text(), "foo bar");
}

#[test]
fn ctrl_w_stays_within_the_current_line_when_it_has_content() {
    let mut e = editor();
    e.set_text("line one\nline two");
    ctrl_w(&mut e);
    assert_eq!(e.get_text(), "line one\nline ");
}

#[test]
fn ctrl_w_at_column_zero_merges_with_the_previous_line() {
    let mut e = editor();
    e.set_text("line one\n");
    // Cursor lands at column 0 of the empty second line after set_text.
    ctrl_w(&mut e);
    assert_eq!(e.get_text(), "line one");
}

#[test]
fn ctrl_w_treats_emoji_graphemes_as_word_characters() {
    let mut e = editor();
    e.set_text("foo 😀😀 bar");
    ctrl_w(&mut e);
    assert_eq!(e.get_text(), "foo 😀😀 ");

    ctrl_w(&mut e);
    assert_eq!(e.get_text(), "foo ");
}

#[test]
fn alt_backspace_is_an_alias_for_ctrl_w() {
    let mut e = editor();
    e.set_text("foo bar");
    e.handle_input(&Key::alt_backspace());
    assert_eq!(e.get_text(), "foo ");
}

// ---------------------------------------------------------------------------
// Word navigation: Ctrl+Left / Ctrl+Right
// ---------------------------------------------------------------------------

fn ctrl_left(e: &mut Editor) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    e.handle_input(&aj_tui::keys::InputEvent::Key(KeyEvent::new(
        KeyCode::Left,
        KeyModifiers::CONTROL,
    )));
}

fn ctrl_right(e: &mut Editor) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    e.handle_input(&aj_tui::keys::InputEvent::Key(KeyEvent::new(
        KeyCode::Right,
        KeyModifiers::CONTROL,
    )));
}

#[test]
fn ctrl_left_walks_word_then_punctuation_then_word_runs() {
    let mut e = editor();
    e.set_text("foo bar... baz");
    // Cursor is at end (col 14).

    ctrl_left(&mut e);
    assert_eq!(e.cursor(), (0, 11), "after 'baz'");

    ctrl_left(&mut e);
    assert_eq!(e.cursor(), (0, 7), "after '...'");

    ctrl_left(&mut e);
    assert_eq!(e.cursor(), (0, 4), "after 'foo '");
}

#[test]
fn ctrl_right_walks_word_then_punctuation_then_word_runs() {
    let mut e = editor();
    e.set_text("foo bar... baz");
    e.handle_input(&Key::ctrl('a')); // line start (cursor at col 0)

    ctrl_right(&mut e);
    assert_eq!(e.cursor(), (0, 3), "end of 'foo'");

    ctrl_right(&mut e);
    assert_eq!(e.cursor(), (0, 7), "end of 'bar'");

    ctrl_right(&mut e);
    assert_eq!(e.cursor(), (0, 10), "end of '...'");

    ctrl_right(&mut e);
    assert_eq!(e.cursor(), (0, 14), "end of 'baz'");
}

#[test]
fn ctrl_right_skips_leading_whitespace_and_lands_after_the_word() {
    let mut e = editor();
    e.set_text("   foo bar");
    e.handle_input(&Key::ctrl('a'));
    ctrl_right(&mut e);
    assert_eq!(e.cursor(), (0, 6), "after '   foo'");
}
