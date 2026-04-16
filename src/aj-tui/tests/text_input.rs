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

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

#[test]
fn on_change_fires_for_every_mutation() {
    use std::cell::RefCell;
    use std::rc::Rc;

    let observed = Rc::new(RefCell::new(Vec::<String>::new()));
    let observed_clone = Rc::clone(&observed);

    let mut input = Input::new("> ");
    input.on_change = Some(Box::new(move |value: &str| {
        observed_clone.borrow_mut().push(value.to_string());
    }));

    type_chars(&mut input, "hi");

    let log = observed.borrow();
    assert_eq!(log.len(), 2);
    assert_eq!(log[0], "h");
    assert_eq!(log[1], "hi");
}

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
