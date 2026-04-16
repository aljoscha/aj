//! Tests for the single-line `Input` component's editing behavior.
//!
//! Covers typing, cursor navigation (arrow keys, word-jumps, line-start
//! / line-end), deletion (backspace, delete, kill-to-end, kill-to-start,
//! kill-word), yank/yank-pop, undo, and Paste events. Each test drives
//! the component through `handle_input` and asserts on `value()` and
//! `cursor()`.

mod support;

use aj_tui::component::Component;
use aj_tui::components::text_input::Input;
use aj_tui::keys::{InputEvent, Key};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn send(input: &mut Input, event: InputEvent) {
    input.handle_input(&event);
}

fn type_chars(input: &mut Input, text: &str) {
    for c in text.chars() {
        send(input, Key::char(c));
    }
}

fn seeded(initial: &str) -> Input {
    let mut input = Input::new("> ");
    input.set_value(initial);
    // F21: `set_value` clamps the existing cursor instead of moving
    // it to end (matches pi-tui). Existing tests are written against
    // the natural "type a value, cursor lands at end" expectation, so
    // the helper sends Ctrl+E (default `tui.editor.cursorLineEnd`) to
    // park the cursor at end-of-value after seeding. Tests that need
    // a different starting position still re-position explicitly via
    // `Ctrl+A`, `Key::left()`, etc., as before.
    send(&mut input, Key::ctrl('e'));
    input
}

// ---------------------------------------------------------------------------
// Typing
// ---------------------------------------------------------------------------

#[test]
fn typing_characters_appends_to_the_value() {
    let mut input = Input::new("> ");
    type_chars(&mut input, "hello");
    assert_eq!(input.value(), "hello");
    assert_eq!(input.cursor(), 5);
}

#[test]
fn typing_after_moving_the_cursor_inserts_at_the_cursor() {
    let mut input = seeded("hlo");
    // Cursor is at end (3); move left twice to land between h and l.
    send(&mut input, Key::left());
    send(&mut input, Key::left());
    type_chars(&mut input, "el");
    assert_eq!(input.value(), "hello");
}

// ---------------------------------------------------------------------------
// Cursor navigation
// ---------------------------------------------------------------------------

#[test]
fn left_and_right_move_by_graphemes() {
    let mut input = seeded("abc");
    send(&mut input, Key::left());
    assert_eq!(input.cursor(), 2);
    send(&mut input, Key::left());
    assert_eq!(input.cursor(), 1);
    send(&mut input, Key::right());
    assert_eq!(input.cursor(), 2);
}

#[test]
fn home_and_end_jump_to_boundaries() {
    let mut input = seeded("hello");
    send(&mut input, Key::home());
    assert_eq!(input.cursor(), 0);
    send(&mut input, Key::end());
    assert_eq!(input.cursor(), 5);
}

#[test]
fn ctrl_a_and_ctrl_e_jump_to_boundaries_too() {
    let mut input = seeded("hello");
    send(&mut input, Key::ctrl('a'));
    assert_eq!(input.cursor(), 0);
    send(&mut input, Key::ctrl('e'));
    assert_eq!(input.cursor(), 5);
}

#[test]
fn ctrl_left_and_ctrl_right_move_by_word() {
    let mut input = seeded("hello world foo");
    send(&mut input, Key::home());

    // ctrl+right: move to the boundary after "hello".
    send(
        &mut input,
        InputEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    assert_eq!(input.cursor(), 5);

    // ctrl+right again lands after "world".
    send(
        &mut input,
        InputEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    assert_eq!(input.cursor(), 11);

    // ctrl+left walks back to the boundary before "world".
    send(
        &mut input,
        InputEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
    assert_eq!(input.cursor(), 6);
}

// Word-motion three-class model (whitespace / punctuation / word) — the
// same model the multi-line `Editor` uses, sharing
// `aj_tui::word_boundary` helpers under the hood. A run of ASCII
// punctuation breaks word jumps the same way whitespace does, so
// `foo bar...` is three "words" (`foo`, `bar`, `...`).

fn ctrl_left(input: &mut Input) {
    send(
        input,
        InputEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
}

fn ctrl_right(input: &mut Input) {
    send(
        input,
        InputEvent::Key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Right,
            crossterm::event::KeyModifiers::CONTROL,
        )),
    );
}

#[test]
fn ctrl_right_walks_word_then_punctuation_then_word_runs() {
    let mut input = seeded("foo bar... baz");
    send(&mut input, Key::home());

    ctrl_right(&mut input);
    assert_eq!(input.cursor(), 3, "end of foo");

    ctrl_right(&mut input);
    assert_eq!(input.cursor(), 7, "end of bar");

    ctrl_right(&mut input);
    assert_eq!(input.cursor(), 10, "end of ...");

    ctrl_right(&mut input);
    assert_eq!(input.cursor(), 14, "end of baz");
}

#[test]
fn ctrl_left_walks_word_then_punctuation_then_word_runs() {
    let mut input = seeded("foo bar... baz");
    // Cursor is already at end (14).

    ctrl_left(&mut input);
    assert_eq!(input.cursor(), 11, "before baz");

    ctrl_left(&mut input);
    assert_eq!(input.cursor(), 7, "before ...");

    ctrl_left(&mut input);
    assert_eq!(input.cursor(), 4, "before bar");
}

#[test]
fn ctrl_w_treats_punctuation_run_as_its_own_word() {
    let mut input = seeded("foo bar...");
    // Cursor is at end. First Ctrl+W kills only the `...` run.
    send(&mut input, Key::ctrl('w'));
    assert_eq!(input.value(), "foo bar");
    // Second Ctrl+W kills "bar".
    send(&mut input, Key::ctrl('w'));
    assert_eq!(input.value(), "foo ");
}

// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------

#[test]
fn backspace_deletes_the_character_before_the_cursor() {
    let mut input = seeded("hello");
    send(&mut input, Key::backspace());
    assert_eq!(input.value(), "hell");
    assert_eq!(input.cursor(), 4);
}

#[test]
fn delete_removes_the_character_after_the_cursor() {
    let mut input = seeded("hello");
    send(&mut input, Key::home());
    send(&mut input, Key::delete());
    assert_eq!(input.value(), "ello");
    assert_eq!(input.cursor(), 0);
}

#[test]
fn ctrl_d_at_end_of_value_does_not_delete() {
    // `ctrl+d` on an empty input is commonly used as EOF/cancel; the
    // component itself just no-ops.
    let mut input = Input::new("> ");
    send(&mut input, Key::ctrl('d'));
    assert_eq!(input.value(), "");
}

#[test]
fn ctrl_d_in_middle_of_value_deletes_forward() {
    let mut input = seeded("hello");
    send(&mut input, Key::home());
    send(&mut input, Key::ctrl('d'));
    assert_eq!(input.value(), "ello");
    assert_eq!(input.cursor(), 0);
}

#[test]
fn ctrl_k_kills_from_cursor_to_end_of_line() {
    let mut input = seeded("hello world");
    // Cursor at position 5 (between "hello" and " world").
    send(&mut input, Key::home());
    for _ in 0..5 {
        send(&mut input, Key::right());
    }
    send(&mut input, Key::ctrl('k'));
    assert_eq!(input.value(), "hello");
    assert_eq!(input.cursor(), 5);
}

#[test]
fn ctrl_u_kills_from_cursor_to_start_of_line() {
    let mut input = seeded("hello world");
    // Move to after "hello ".
    send(&mut input, Key::home());
    for _ in 0..6 {
        send(&mut input, Key::right());
    }
    send(&mut input, Key::ctrl('u'));
    assert_eq!(input.value(), "world");
    assert_eq!(input.cursor(), 0);
}

#[test]
fn ctrl_w_kills_word_backward() {
    let mut input = seeded("hello world");
    // Cursor at end.
    send(&mut input, Key::ctrl('w'));
    assert_eq!(input.value(), "hello ");
    // A second ctrl+w removes "hello " too.
    send(&mut input, Key::ctrl('w'));
    assert_eq!(input.value(), "");
}

#[test]
fn alt_d_kills_word_forward() {
    let mut input = seeded("hello world");
    send(&mut input, Key::home());
    send(&mut input, Key::alt('d'));
    assert_eq!(input.value(), " world");
    assert_eq!(input.cursor(), 0);
}

// ---------------------------------------------------------------------------
// Kill ring + yank
// ---------------------------------------------------------------------------

#[test]
fn ctrl_y_yanks_the_most_recently_killed_text() {
    let mut input = seeded("hello world");
    // Kill "world" with ctrl+w.
    send(&mut input, Key::ctrl('w'));
    assert_eq!(input.value(), "hello ");
    // Move to start, yank.
    send(&mut input, Key::home());
    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "worldhello ");
}

// NOTE: no test for Alt+y / yank-pop yet — the Input component doesn't wire
// `KillRing::rotate` to a keybinding. Add the test alongside that feature.

// ---------------------------------------------------------------------------
// Undo
// ---------------------------------------------------------------------------

#[test]
fn ctrl_minus_undoes_the_last_edit() {
    let mut input = seeded("hello");
    send(&mut input, Key::backspace());
    assert_eq!(input.value(), "hell");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "hello");
}

#[test]
fn undo_after_kill_restores_the_killed_content() {
    let mut input = seeded("hello world");
    send(&mut input, Key::ctrl('u'));
    assert_eq!(input.value(), "");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "hello world");
}

// ---------------------------------------------------------------------------
// Paste
// ---------------------------------------------------------------------------

#[test]
fn paste_event_inserts_at_the_cursor() {
    let mut input = seeded("hi");
    send(&mut input, Key::home());
    send(&mut input, InputEvent::Paste("PASTED ".to_string()));
    assert_eq!(input.value(), "PASTED hi");
    assert_eq!(input.cursor(), 7);
}

#[test]
fn paste_with_tabs_expands_each_tab_to_four_spaces() {
    // A pasted tab is an "indent the user wanted to keep" signal —
    // expand to four spaces (matching the Editor's paste-tab handling)
    // rather than collapsing to one space.
    let mut input = Input::new("> ");
    input.handle_input(&InputEvent::Paste("\thi\tthere".to_string()));
    assert_eq!(input.value(), "    hi    there");
    assert_eq!(input.cursor(), "    hi    there".len());
}

#[test]
fn paste_strips_other_control_chars_but_keeps_tabs_expanded() {
    // Newlines, NUL, etc. are dropped (Input is single-line). Tabs
    // are the one control char we expand rather than strip.
    let mut input = Input::new("> ");
    input.handle_input(&InputEvent::Paste("a\tb\nc\0d".to_string()));
    assert_eq!(input.value(), "a    bcd");
}

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

#[test]
fn on_submit_fires_on_enter_and_passes_the_current_value() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let submitted = Rc::new(RefCell::new(None::<String>));
    let submitted_clone = Rc::clone(&submitted);

    let mut input = Input::new("> ");
    input.set_value("hello");
    input.on_submit = Some(Box::new(move |v: &str| {
        *submitted_clone.borrow_mut() = Some(v.to_string());
    }));

    send(&mut input, Key::enter());

    assert_eq!(submitted.borrow().as_deref(), Some("hello"));
}

#[test]
fn enter_on_value_with_trailing_backslash_submits_the_string_verbatim() {
    // The multi-line `Editor` treats `\\` + Enter as "insert newline,
    // don't submit" (a workaround for terminals that can't
    // distinguish Shift+Enter from plain Enter). The single-line
    // `Input` must NOT inherit that behavior: a trailing backslash is
    // just a character in the value and Enter submits as normal.
    use std::cell::RefCell;
    use std::rc::Rc;

    let submitted = Rc::new(RefCell::new(None::<String>));
    let submitted_clone = Rc::clone(&submitted);

    let mut input = Input::new("");
    // Type the whole string character-by-character so the backslash
    // goes through the regular char-insert path.
    for c in "path\\".chars() {
        send(&mut input, Key::char(c));
    }
    input.on_submit = Some(Box::new(move |v: &str| {
        *submitted_clone.borrow_mut() = Some(v.to_string());
    }));

    send(&mut input, Key::enter());

    assert_eq!(
        submitted.borrow().as_deref(),
        Some("path\\"),
        "Input submits the value verbatim, including a trailing backslash",
    );
}

// ---------------------------------------------------------------------------
// Unicode
// ---------------------------------------------------------------------------

#[test]
fn cursor_moves_by_graphemes_across_multibyte_input() {
    let mut input = seeded("héllo");
    // "héllo" is 6 bytes: h=1, é=2, l=1, l=1, o=1. Cursor at 6.
    send(&mut input, Key::left());
    assert_eq!(input.cursor(), 5); // before 'o'
    send(&mut input, Key::left()); // before 'l'
    assert_eq!(input.cursor(), 4);
    send(&mut input, Key::left()); // before 'l'
    assert_eq!(input.cursor(), 3);
    send(&mut input, Key::left()); // before 'é' (skipping 2 bytes)
    assert_eq!(input.cursor(), 1);
}

// ---------------------------------------------------------------------------
// Kill ring behavior
// ---------------------------------------------------------------------------

#[test]
fn ctrl_y_does_nothing_when_the_kill_ring_is_empty() {
    let mut input = seeded("test");
    send(&mut input, Key::ctrl('e'));
    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "test");
}

#[test]
fn consecutive_ctrl_w_accumulates_into_one_kill_ring_entry() {
    // Three Ctrl+W back-to-back kill "three", "two ", "one " in that
    // order (each cuts the word before the cursor). Accumulation
    // prepends each new deletion so the single ring entry ends up as
    // "one two three". Yanking then pastes the full sequence.
    let mut input = seeded("one two three");
    send(&mut input, Key::ctrl('e'));

    send(&mut input, Key::ctrl('w'));
    send(&mut input, Key::ctrl('w'));
    send(&mut input, Key::ctrl('w'));
    assert_eq!(input.value(), "");

    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "one two three");
}

#[test]
fn non_kill_actions_break_kill_accumulation() {
    let mut input = seeded("foo bar baz");
    send(&mut input, Key::ctrl('e'));
    send(&mut input, Key::ctrl('w')); // Delete "baz"
    assert_eq!(input.value(), "foo bar ");

    // Typing a character breaks the accumulation chain.
    type_chars(&mut input, "x");
    assert_eq!(input.value(), "foo bar x");

    send(&mut input, Key::ctrl('w')); // Deletes "x" into a new entry
    assert_eq!(input.value(), "foo bar ");

    send(&mut input, Key::ctrl('y')); // Most recent is "x"
    assert_eq!(input.value(), "foo bar x");
}

#[test]
fn backward_and_forward_deletions_compose_during_accumulation() {
    // Cursor at "|" in "prefix|suffix". Ctrl+K deletes forward
    // ("|suffix"), then Ctrl+Y pastes it intact.
    let mut input = seeded("prefix|suffix");
    send(&mut input, Key::ctrl('a'));
    for _ in 0..6 {
        send(&mut input, Key::right());
    }

    send(&mut input, Key::ctrl('k'));
    assert_eq!(input.value(), "prefix");

    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "prefix|suffix");
}

#[test]
fn alt_d_kills_successive_words_and_accumulates_them() {
    let mut input = seeded("hello world test");
    send(&mut input, Key::ctrl('a'));

    send(&mut input, Key::alt('d')); // deletes "hello"
    assert_eq!(input.value(), " world test");

    send(&mut input, Key::alt('d')); // deletes " world"
    assert_eq!(input.value(), " test");

    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "hello world test");
}

#[test]
fn yank_in_the_middle_of_text_inserts_at_the_cursor() {
    let mut input = seeded("word");
    send(&mut input, Key::ctrl('e'));
    send(&mut input, Key::ctrl('w')); // kill "word"

    input.set_value("hello world");
    send(&mut input, Key::ctrl('a'));
    for _ in 0..6 {
        send(&mut input, Key::right());
    }

    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "hello wordworld");
}

// ---------------------------------------------------------------------------
// Alt+Y yank-pop
// ---------------------------------------------------------------------------

fn kill_three_entries(input: &mut Input) {
    // Set the ring to [first, second, third] with "third" as the
    // most-recent entry.
    for word in ["first", "second", "third"] {
        input.set_value(word);
        send(input, Key::ctrl('e'));
        send(input, Key::ctrl('w'));
    }
}

#[test]
fn alt_y_cycles_through_kill_ring_after_ctrl_y() {
    let mut input = Input::new("> ");
    kill_three_entries(&mut input);
    assert_eq!(input.value(), "");

    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "third");

    send(&mut input, Key::alt('y'));
    assert_eq!(input.value(), "second");

    send(&mut input, Key::alt('y'));
    assert_eq!(input.value(), "first");

    send(&mut input, Key::alt('y'));
    assert_eq!(input.value(), "third", "cycles back to the most recent");
}

#[test]
fn alt_y_is_a_noop_when_not_preceded_by_a_yank() {
    let mut input = seeded("test");
    send(&mut input, Key::ctrl('e'));
    send(&mut input, Key::ctrl('w')); // kill "test"
    input.set_value("other");
    send(&mut input, Key::ctrl('e'));

    // Typing breaks the yank chain before Alt+Y can act.
    type_chars(&mut input, "x");
    assert_eq!(input.value(), "otherx");

    send(&mut input, Key::alt('y'));
    assert_eq!(input.value(), "otherx", "no yank happened, nothing to pop");
}

#[test]
fn alt_y_is_a_noop_when_the_kill_ring_has_a_single_entry() {
    let mut input = seeded("only");
    send(&mut input, Key::ctrl('e'));
    send(&mut input, Key::ctrl('w'));

    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "only");

    send(&mut input, Key::alt('y'));
    assert_eq!(input.value(), "only", "single entry → nothing to rotate to");
}

#[test]
fn non_yank_actions_break_the_alt_y_chain() {
    let mut input = Input::new("> ");
    kill_three_entries(&mut input);

    send(&mut input, Key::ctrl('y')); // "third"
    type_chars(&mut input, "x"); // breaks the yank chain
    assert_eq!(input.value(), "thirdx");

    send(&mut input, Key::alt('y'));
    assert_eq!(
        input.value(),
        "thirdx",
        "non-yank action must stop the chain"
    );
}

#[test]
fn yank_pop_in_the_middle_of_text_replaces_the_yanked_span() {
    let mut input = Input::new("> ");

    // Two entries in the ring.
    input.set_value("FIRST");
    send(&mut input, Key::ctrl('e'));
    send(&mut input, Key::ctrl('w'));
    input.set_value("SECOND");
    send(&mut input, Key::ctrl('e'));
    send(&mut input, Key::ctrl('w'));

    input.set_value("hello world");
    send(&mut input, Key::ctrl('a'));
    for _ in 0..6 {
        send(&mut input, Key::right());
    }

    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "hello SECONDworld");

    send(&mut input, Key::alt('y'));
    assert_eq!(input.value(), "hello FIRSTworld");
}

#[test]
fn undo_after_yank_pop_reverts_the_rotation_as_its_own_step() {
    // F38: yank-pop pushes its own undo snapshot before rotating, so
    // an undo after a yank-pop lands on the previously-yanked content
    // (the state visible just before this yank-pop fired) rather than
    // collapsing all the way back to the pre-yank empty state.
    let mut input = Input::new("> ");

    // Two entries in the ring: [FIRST, SECOND], peek = "SECOND".
    input.set_value("FIRST");
    send(&mut input, Key::ctrl('e'));
    send(&mut input, Key::ctrl('w'));
    input.set_value("SECOND");
    send(&mut input, Key::ctrl('e'));
    send(&mut input, Key::ctrl('w'));
    assert_eq!(input.value(), "");

    // Yank surfaces the most recent entry.
    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "SECOND");

    // Yank-pop rotates the ring and replaces the yanked text.
    send(&mut input, Key::alt('y'));
    assert_eq!(input.value(), "FIRST");

    // Undo must revert just the rotation, leaving the originally
    // yanked text in place.
    send(&mut input, Key::ctrl('-'));
    assert_eq!(
        input.value(),
        "SECOND",
        "undo after yank-pop reverts the rotation, not the entire yank"
    );
    assert_eq!(input.cursor(), "SECOND".len());

    // A second undo collapses the original yank back to the pre-yank
    // empty state, the same as undoing a plain yank.
    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "");
}

#[test]
fn undo_after_chained_yank_pops_steps_back_one_rotation_at_a_time() {
    // F38 (chain): each yank-pop in a chain pushes its own snapshot,
    // so undoing walks the rotation history backward one entry per
    // step rather than jumping past all the rotations at once.
    let mut input = Input::new("> ");
    kill_three_entries(&mut input); // ring = [first, second, third]

    send(&mut input, Key::ctrl('y'));
    assert_eq!(input.value(), "third");
    send(&mut input, Key::alt('y'));
    assert_eq!(input.value(), "second");
    send(&mut input, Key::alt('y'));
    assert_eq!(input.value(), "first");

    // Three undos: rotate-back, rotate-back, then the original yank.
    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "second");
    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "third");
    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "");
}

// ---------------------------------------------------------------------------
// Undo coalescing and boundary cases
// ---------------------------------------------------------------------------

#[test]
fn undo_coalesces_consecutive_word_characters_into_one_unit() {
    // Typing a word coalesces into one undo unit, but a space breaks
    // coalescing: "hello world" undoes in two steps, " world" then
    // "hello".
    let mut input = Input::new("> ");
    type_chars(&mut input, "hello world");
    assert_eq!(input.value(), "hello world");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "hello");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "");
}

#[test]
fn undo_removes_spaces_one_at_a_time() {
    let mut input = Input::new("> ");
    type_chars(&mut input, "hello  ");
    assert_eq!(input.value(), "hello  ");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "hello ");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "hello");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "");
}

#[test]
fn undo_reverts_backspace() {
    let mut input = Input::new("> ");
    type_chars(&mut input, "hello");
    send(&mut input, Key::backspace());
    assert_eq!(input.value(), "hell");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "hello");
}

#[test]
fn undo_reverts_forward_delete() {
    let mut input = seeded("hello");
    send(&mut input, Key::ctrl('a'));
    send(&mut input, Key::right());
    send(&mut input, Key::delete());
    assert_eq!(input.value(), "hllo");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "hello");
}

#[test]
fn undo_reverts_a_yank() {
    let mut input = Input::new("> ");
    type_chars(&mut input, "hello ");
    send(&mut input, Key::ctrl('w')); // kill "hello "
    send(&mut input, Key::ctrl('y')); // yank it back
    assert_eq!(input.value(), "hello ");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "", "undo should remove the yanked span");
}

#[test]
fn undo_reverts_alt_d_kill_word_forward() {
    let mut input = seeded("hello world");
    send(&mut input, Key::ctrl('a'));
    send(&mut input, Key::alt('d'));
    assert_eq!(input.value(), " world");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "hello world");
}

#[test]
fn undo_reverts_a_paste_atomically() {
    // A bracketed-paste event is one undo unit regardless of length.
    let mut input = seeded("hello world");
    send(&mut input, Key::ctrl('a'));
    for _ in 0..5 {
        send(&mut input, Key::right());
    }

    input.handle_input(&InputEvent::Paste("beep boop".to_string()));
    assert_eq!(input.value(), "hellobeep boop world");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "hello world");
}

#[test]
fn cursor_movement_starts_a_new_undo_unit() {
    let mut input = Input::new("> ");
    type_chars(&mut input, "abc");

    // Cursor movement breaks the typing-word coalescing.
    send(&mut input, Key::ctrl('a'));
    send(&mut input, Key::ctrl('e'));

    type_chars(&mut input, "de");
    assert_eq!(input.value(), "abcde");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(
        input.value(),
        "abc",
        "undo peels off only the post-move 'de'"
    );

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "");
}

// ---------------------------------------------------------------------------
// F22: undo-coalescing transitions (pi-tui parity)
// ---------------------------------------------------------------------------
//
// Pi-tui's `Input.insertCharacter` uses one rule for undo coalescing:
// every whitespace insert pushes a fresh snapshot, every non-whitespace
// insert pushes only when `lastAction !== "type-word"`, and after every
// insert `lastAction` is set to `"type-word"`. The Rust port previously
// drove an extra `TypeWhitespace` state through a 2x4 match table that
// produced identical observable behavior; F22 collapsed that to the
// single-state form and these tests pin the cross-state transitions
// explicitly so a future refactor can't drift unnoticed.

#[test]
fn typing_a_word_after_a_space_extends_the_spaces_undo_unit_not_a_new_one() {
    // Pi-tui's `lastAction = "type-word"` after a whitespace insert is
    // the load-bearing line: it makes the next non-whitespace
    // character coalesce into the snapshot the space pushed, so
    // "hello world" produces exactly two undo units (before 'h',
    // before ' '), and a single undo cleanly removes " world".
    let mut input = Input::new("> ");
    type_chars(&mut input, "hello world");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(
        input.value(),
        "hello",
        "the second undo unit covers the space + 'world' suffix"
    );

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "");
}

#[test]
fn typing_after_a_kill_starts_a_fresh_undo_unit() {
    // A kill-word leaves `last_action = Kill`, so the next typed
    // character pushes its own snapshot rather than coalescing into
    // the kill's snapshot. Three undos walk back through: typed
    // suffix, kill, original seed.
    let mut input = seeded("hello ");
    send(&mut input, Key::ctrl('w')); // kill "hello "
    assert_eq!(input.value(), "");

    type_chars(&mut input, "world");
    assert_eq!(input.value(), "world");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(
        input.value(),
        "",
        "undo peels off only the post-kill 'world'"
    );

    send(&mut input, Key::ctrl('-'));
    assert_eq!(
        input.value(),
        "hello ",
        "the next undo restores the killed span"
    );
}

#[test]
fn typing_after_a_yank_starts_a_fresh_undo_unit() {
    // A yank leaves `last_action = Yank`, so the next typed character
    // pushes a fresh snapshot. Without that, "kill, yank, type" would
    // collapse the typed suffix into the yank's snapshot and lose
    // a discrete undo step.
    let mut input = seeded("hello ");
    send(&mut input, Key::ctrl('w')); // kill "hello "
    send(&mut input, Key::ctrl('y')); // yank it back
    assert_eq!(input.value(), "hello ");

    type_chars(&mut input, "world");
    assert_eq!(input.value(), "hello world");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(
        input.value(),
        "hello ",
        "undo peels off the post-yank 'world' as its own unit"
    );
}

#[test]
fn typing_after_a_backspace_starts_a_fresh_undo_unit() {
    // A backspace resets `last_action` to `None`, so the next typed
    // character pushes a fresh snapshot rather than rolling forward
    // into the typing run that preceded the delete.
    let mut input = Input::new("> ");
    type_chars(&mut input, "hello");
    send(&mut input, Key::backspace());
    assert_eq!(input.value(), "hell");

    type_chars(&mut input, "p");
    assert_eq!(input.value(), "hellp");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(
        input.value(),
        "hell",
        "undo peels off only the post-backspace 'p'"
    );
}

#[test]
fn each_whitespace_insert_pushes_its_own_snapshot_even_with_words_between() {
    // "a  b" walks every transition the rule cares about: word-from-
    // empty (push), space-after-word (push), space-after-space (push),
    // word-after-space (no push, coalesces). Three undo units total.
    let mut input = Input::new("> ");
    type_chars(&mut input, "a  b");
    assert_eq!(input.value(), "a  b");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(
        input.value(),
        "a ",
        "the third unit covers the second space + 'b'"
    );

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "a", "the second unit covers the first space");

    send(&mut input, Key::ctrl('-'));
    assert_eq!(input.value(), "");
}

// ---------------------------------------------------------------------------
// Render: wide / CJK / fullwidth handling
// ---------------------------------------------------------------------------
//
// The `Input` component claims to clamp its rendered line's visible
// width to the `width` argument. The risk path is full-width and CJK
// characters: each grapheme reports visible width 2, so naive
// byte/char slicing can drift and overflow. These tests pin the
// invariant at cursor positions that exercise the horizontal-scroll
// logic (start / middle / end) and across four scripts that all
// stress the width-2 path.

#[test]
fn render_clamps_line_width_for_wide_cjk_and_fullwidth_strings() {
    use aj_tui::ansi::visible_width;

    let samples: &[(&str, &str)] = &[
        ("Hangul", "가나다라마바사아자차카타파하"),
        ("Japanese", "あいうえおかきくけこさしすせそたち"),
        ("Chinese", "你好世界这是一个测试字符串例子演示"),
        ("Fullwidth", "ＡＢＣＤＥＦＧＨＩＪＫＬＭＮＯＰＱ"),
    ];
    let widths = [10usize, 20, 40];

    for (label, text) in samples {
        for width in widths {
            let mut input = Input::new("");
            input.set_value(text);

            // Three cursor positions: start, middle, end.
            for position_label in ["start", "middle", "end"] {
                match position_label {
                    "start" => {
                        send(&mut input, Key::ctrl('a'));
                    }
                    "middle" => {
                        send(&mut input, Key::ctrl('a'));
                        for _ in 0..(text.chars().count() / 2) {
                            send(&mut input, Key::right());
                        }
                    }
                    _ => {
                        send(&mut input, Key::ctrl('e'));
                    }
                }

                let lines = input.render(width);
                assert_eq!(lines.len(), 1, "Input always renders one line");
                let vw = visible_width(&lines[0]);
                assert!(
                    vw <= width,
                    "[{}] rendered width {} exceeds width cap {} at cursor={}; line={:?}",
                    label,
                    vw,
                    width,
                    position_label,
                    lines[0],
                );
            }
        }
    }
}

#[test]
fn render_keeps_the_cursor_visible_when_horizontally_scrolling_wide_text() {
    // A wide CJK string at width 20 forces horizontal scrolling. The
    // `sliceByColumn`-based scrolling logic in `render` must center
    // the cursor in the visible window, not leave it past the right
    // edge where a fake-cursor inverse-video sequence would land off
    // the end of the string.
    use aj_tui::ansi::visible_width;

    let mut input = Input::new("");
    input.set_value("가나다라마바사아자차카타파하거너더러머");
    send(&mut input, Key::ctrl('a'));
    for _ in 0..5 {
        send(&mut input, Key::right());
    }

    let width = 20;
    let lines = input.render(width);
    assert_eq!(lines.len(), 1);
    assert!(
        visible_width(&lines[0]) <= width,
        "horizontally scrolled wide text should still fit within the width cap; \
         rendered width = {}, cap = {}, line = {:?}",
        visible_width(&lines[0]),
        width,
        lines[0],
    );
    // The rendered line must contain *some* fragment of the source
    // string (i.e. it wasn't reduced to just the prompt and cursor
    // padding). A width of 20 with two-column graphemes leaves at
    // least ~8 characters' worth of content visible.
    let plain = support::strip_ansi(&lines[0]);
    assert!(
        !plain.trim().is_empty(),
        "rendered line should show at least one character of content; got {:?}",
        lines[0],
    );
}

// ---------------------------------------------------------------------------
// F21: set_value clamps the existing cursor (pi-tui parity)
// ---------------------------------------------------------------------------

#[test]
fn set_value_keeps_existing_cursor_when_within_new_length() {
    // Cursor 4 sits inside the new value's length 9, so `set_value`
    // leaves it where it is. Without F21 it snapped to end (9),
    // which dropped the user's caret out from under them on a
    // mid-edit value swap.
    let mut input = seeded("abcdef");
    send(&mut input, Key::left());
    send(&mut input, Key::left());
    assert_eq!(input.cursor(), 4);

    input.set_value("123456789");
    assert_eq!(input.value(), "123456789");
    assert_eq!(input.cursor(), 4);
}

#[test]
fn set_value_clamps_existing_cursor_to_new_length() {
    // Cursor 5 (end of "hello") exceeds the new length 2, so
    // `set_value` clamps it to 2 (the new end). Pi-tui:
    // `Math.min(this.cursor, value.length)`.
    let mut input = seeded("hello");
    assert_eq!(input.cursor(), 5);

    input.set_value("hi");
    assert_eq!(input.value(), "hi");
    assert_eq!(input.cursor(), 2);
}

#[test]
fn set_value_on_a_freshly_constructed_input_keeps_cursor_at_zero() {
    // Pre-F21 behavior was cursor-to-end; pi-tui clamps. A fresh
    // `Input` starts with cursor = 0, so `set_value("hello")` leaves
    // it at 0, not 5. Callers that want cursor-at-end should
    // explicitly seek (e.g., Ctrl+E).
    let mut input = Input::new("> ");
    input.set_value("hello");
    assert_eq!(input.value(), "hello");
    assert_eq!(input.cursor(), 0);
}

#[test]
fn set_value_snaps_clamped_cursor_off_a_mid_codepoint_byte_offset() {
    // Defensive Rust-side behavior beyond strict pi parity. Pi works
    // in JS code units and accepts any clamped index; our cursor is
    // a UTF-8 byte offset and would panic on later string slicing if
    // it landed mid-multi-byte. `set_value` snaps a clamped offset
    // *forward* to the next char boundary so the cursor's logical
    // position lands "past" the disrupted codepoint, matching the
    // analogous position pi's UTF-16 clamp would have produced.
    //
    // Setup: cursor at byte 4 in "abcdef" (between 'd' and 'e').
    // New value "abcé" has bytes a(0..1), b(1..2), c(2..3), é(3..5)
    // for a total length of 5. Clamping gives `min(4, 5) = 4`, which
    // lies in the middle of the two-byte é codepoint. The snap pulls
    // cursor *forward* to byte 5 (the boundary after é, which is also
    // end-of-string). This matches pi's behavior in the parallel
    // UTF-16 case where the clamped code-unit offset would land at
    // position 4 (= end of "abcé" since é is one BMP code unit there).
    let mut input = seeded("abcdef");
    send(&mut input, Key::left());
    send(&mut input, Key::left());
    assert_eq!(input.cursor(), 4);

    input.set_value("abcé");
    assert_eq!(input.value(), "abcé");
    assert_eq!(input.cursor(), 5);
}

#[test]
fn set_value_snap_forward_keeps_cursor_after_partial_multibyte_in_middle() {
    // Snap-forward sanity for a clamped offset that's *not* at end of
    // string. Old cursor at byte 3 in "abc_de" (between 'c' and '_').
    // New value "ab" + "é" + "fgh" = "abéfgh" has bytes
    // a(0..1), b(1..2), é(2..4), f(4..5), g(5..6), h(6..7). Clamping
    // gives `min(3, 7) = 3`, mid-é. Snap forward to byte 4 = "after
    // é, before f" — semantically the same position pi's UTF-16 clamp
    // (`min(3, 6) = 3`) would put the cursor: past the é, before the
    // f. A subsequent type lands the new char between é and f.
    let mut input = seeded("abc_de");
    // Move cursor left three times so it sits at byte 3 (between 'c'
    // and '_'). seeded() left it at 6 (end of value).
    send(&mut input, Key::left());
    send(&mut input, Key::left());
    send(&mut input, Key::left());
    assert_eq!(input.cursor(), 3);

    input.set_value("abéfgh");
    assert_eq!(input.value(), "abéfgh");
    assert_eq!(input.cursor(), 4);

    // Verify the cursor is positioned correctly by typing a marker
    // and checking it lands between é and f.
    send(&mut input, Key::char('X'));
    assert_eq!(input.value(), "abéXfgh");
}

// ---------------------------------------------------------------------------
// F23: Cursor-position byte-equivalence across wide-CJK / fullwidth typing
// ---------------------------------------------------------------------------
//
// `Input::render` uses a horizontal-scroll heuristic that's simpler
// than pi-tui's three-way column-based branching (start / middle / end)
// — it scrolls in grapheme indices rather than visible columns. Existing
// CJK tests
// (`render_clamps_line_width_for_wide_cjk_and_fullwidth_strings`,
// `render_keeps_the_cursor_visible_when_horizontally_scrolling_wide_text`)
// pin down the rendered-width invariant. The tests below pin the
// orthogonal invariant: the cursor's *underlying byte position* must
// advance by exactly one grapheme's UTF-8 byte length per typed char,
// per arrow-key step, and per backspace, across the four representative
// scripts the existing render tests exercise (Hangul, Japanese,
// Chinese, fullwidth ASCII) plus a mixed ASCII + CJK case. This locks
// in the byte-arithmetic in `insert_char`, `move_left` / `move_right`,
// and `backspace` independent of the simplified scroll heuristic.

#[test]
fn cursor_advances_by_utf8_byte_length_when_typing_hangul() {
    // Each precomposed Hangul syllable is one Unicode scalar, three
    // UTF-8 bytes, two terminal columns wide.
    let hangul = "가나다라마";
    let mut input = Input::new("> ");
    let mut expected = 0usize;
    for c in hangul.chars() {
        send(&mut input, Key::char(c));
        expected += c.len_utf8();
        assert_eq!(
            input.cursor(),
            expected,
            "after typing {:?}, cursor should be at byte {} (got {}); value = {:?}",
            c,
            expected,
            input.cursor(),
            input.value(),
        );
    }
    assert_eq!(input.value(), hangul);
    assert_eq!(input.cursor(), hangul.len());
}

#[test]
fn cursor_advances_by_utf8_byte_length_when_typing_japanese_hiragana() {
    let japanese = "あいうえお";
    let mut input = Input::new("> ");
    let mut expected = 0usize;
    for c in japanese.chars() {
        send(&mut input, Key::char(c));
        expected += c.len_utf8();
        assert_eq!(input.cursor(), expected);
    }
    assert_eq!(input.value(), japanese);
    assert_eq!(input.cursor(), japanese.len());
}

#[test]
fn cursor_advances_by_utf8_byte_length_when_typing_chinese() {
    let chinese = "你好世界";
    let mut input = Input::new("> ");
    let mut expected = 0usize;
    for c in chinese.chars() {
        send(&mut input, Key::char(c));
        expected += c.len_utf8();
        assert_eq!(input.cursor(), expected);
    }
    assert_eq!(input.value(), chinese);
    assert_eq!(input.cursor(), chinese.len());
}

#[test]
fn cursor_advances_by_utf8_byte_length_when_typing_fullwidth_ascii() {
    let fullwidth = "ＡＢＣＤＥ";
    let mut input = Input::new("> ");
    let mut expected = 0usize;
    for c in fullwidth.chars() {
        send(&mut input, Key::char(c));
        expected += c.len_utf8();
        assert_eq!(input.cursor(), expected);
    }
    assert_eq!(input.value(), fullwidth);
    assert_eq!(input.cursor(), fullwidth.len());
}

#[test]
fn cursor_left_arrow_walks_one_grapheme_per_step_through_wide_text() {
    // After typing a wide-CJK string, cursor is at end-of-value. Each
    // left arrow must walk back by exactly one grapheme — for the
    // sample below, three UTF-8 bytes per step.
    let chinese = "你好世界这是";
    let mut input = Input::new("> ");
    type_chars(&mut input, chinese);
    assert_eq!(input.cursor(), chinese.len());

    let mut expected = chinese.len();
    for c in chinese.chars().rev() {
        send(&mut input, Key::left());
        expected -= c.len_utf8();
        assert_eq!(
            input.cursor(),
            expected,
            "after Left over {:?}, cursor should be at byte {} (got {})",
            c,
            expected,
            input.cursor(),
        );
    }
    assert_eq!(input.cursor(), 0);
}

#[test]
fn cursor_right_arrow_walks_one_grapheme_per_step_through_wide_text() {
    let japanese = "あいうえおか";
    let mut input = Input::new("> ");
    type_chars(&mut input, japanese);

    // Park cursor at start.
    send(&mut input, Key::home());
    assert_eq!(input.cursor(), 0);

    let mut expected = 0usize;
    for c in japanese.chars() {
        send(&mut input, Key::right());
        expected += c.len_utf8();
        assert_eq!(input.cursor(), expected);
    }
    assert_eq!(input.cursor(), japanese.len());
}

#[test]
fn backspace_decrements_cursor_by_each_graphemes_byte_length_in_wide_text() {
    // Backspace walks back by one grapheme and removes those bytes
    // from the value. Verify both invariants hold across a Hangul
    // string.
    let hangul = "가나다라";
    let mut input = Input::new("> ");
    type_chars(&mut input, hangul);
    let mut expected = hangul.len();

    // Build the expected suffix-shrunk value at each step.
    let chars: Vec<char> = hangul.chars().collect();
    for (i, c) in chars.iter().enumerate().rev() {
        send(&mut input, Key::backspace());
        expected -= c.len_utf8();
        assert_eq!(input.cursor(), expected);
        // Value should be the prefix of the original up through the
        // i-th char.
        let want: String = chars[..i].iter().collect();
        assert_eq!(input.value(), want);
    }
    assert_eq!(input.cursor(), 0);
    assert_eq!(input.value(), "");
}

#[test]
fn mixed_ascii_and_cjk_typing_keeps_cursor_byte_position_consistent() {
    // Single-byte and multi-byte chars interleave. Each char advances
    // the cursor by its own UTF-8 byte length; arrow-left then walks
    // back one grapheme per step, regardless of whether the grapheme
    // is one byte or three.
    let mixed = "a가b나c다";
    let mut input = Input::new("> ");
    let mut expected = 0usize;
    for c in mixed.chars() {
        send(&mut input, Key::char(c));
        expected += c.len_utf8();
        assert_eq!(input.cursor(), expected);
    }
    assert_eq!(input.value(), mixed);

    for c in mixed.chars().rev() {
        send(&mut input, Key::left());
        expected -= c.len_utf8();
        assert_eq!(input.cursor(), expected);
    }
    assert_eq!(input.cursor(), 0);

    // And forward again: same arithmetic in the other direction.
    for c in mixed.chars() {
        send(&mut input, Key::right());
        expected += c.len_utf8();
        assert_eq!(input.cursor(), expected);
    }
    assert_eq!(input.cursor(), mixed.len());
}
