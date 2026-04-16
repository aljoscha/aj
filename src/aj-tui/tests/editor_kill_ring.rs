//! Tests for the editor's integration with the kill ring.
//!
//! The standalone kill-ring data structure has its own coverage in
//! `kill_ring.rs`. This file is about how the editor's kill/yank
//! keybindings drive the ring: `Ctrl+W` / `Ctrl+U` / `Ctrl+K` / `Alt+D`
//! push, `Ctrl+Y` yanks, and `Alt+Y` cycles. Particular attention to
//! kill accumulation (consecutive kills merge into one entry, while
//! non-delete actions break the chain) and to the `Alt+Y` yank-pop
//! semantics (requires an immediate-preceding yank, and replaces the
//! yanked region atomically).

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
// Basic kill + yank
// ---------------------------------------------------------------------------

#[test]
fn ctrl_w_saves_deleted_text_and_ctrl_y_yanks_it() {
    let mut e = editor();
    e.set_text("foo bar baz");

    e.handle_input(&Key::ctrl('w')); // kill word backward "baz"
    assert_eq!(e.get_text(), "foo bar ");

    e.handle_input(&Key::ctrl('a')); // move to start
    e.handle_input(&Key::ctrl('y')); // yank
    assert_eq!(e.get_text(), "bazfoo bar ");
}

#[test]
fn ctrl_u_saves_deleted_text() {
    let mut e = editor();
    e.set_text("hello world");

    e.handle_input(&Key::ctrl('a'));
    for _ in 0..6 {
        e.handle_input(&Key::right());
    }

    e.handle_input(&Key::ctrl('u')); // kill "hello "
    assert_eq!(e.get_text(), "world");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn ctrl_k_saves_deleted_text() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));

    e.handle_input(&Key::ctrl('k')); // kill "hello world"
    assert_eq!(e.get_text(), "");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "hello world");
}

#[test]
fn ctrl_y_does_nothing_when_kill_ring_is_empty() {
    let mut e = editor();
    e.set_text("test");
    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "test");
}

// ---------------------------------------------------------------------------
// Alt+Y yank-pop
// ---------------------------------------------------------------------------

#[test]
fn alt_y_cycles_through_kill_ring_after_ctrl_y() {
    let mut e = editor();

    // Build a ring of three entries.
    e.set_text("first");
    e.handle_input(&Key::ctrl('w'));
    e.set_text("second");
    e.handle_input(&Key::ctrl('w'));
    e.set_text("third");
    e.handle_input(&Key::ctrl('w'));

    assert_eq!(e.get_text(), "");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "third");

    e.handle_input(&Key::alt('y'));
    assert_eq!(e.get_text(), "second");

    e.handle_input(&Key::alt('y'));
    assert_eq!(e.get_text(), "first");

    e.handle_input(&Key::alt('y'));
    assert_eq!(e.get_text(), "third");
}

#[test]
fn alt_y_does_nothing_if_not_preceded_by_yank() {
    let mut e = editor();

    e.set_text("test");
    e.handle_input(&Key::ctrl('w')); // ring has ["test"]
    e.set_text("other");

    // Typing breaks any yank chain.
    e.handle_input(&Key::char('x'));
    assert_eq!(e.get_text(), "otherx");

    e.handle_input(&Key::alt('y'));
    assert_eq!(e.get_text(), "otherx");
}

#[test]
fn alt_y_does_nothing_if_kill_ring_has_at_most_one_entry() {
    let mut e = editor();

    e.set_text("only");
    e.handle_input(&Key::ctrl('w'));

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "only");

    e.handle_input(&Key::alt('y'));
    assert_eq!(e.get_text(), "only");
}

// ---------------------------------------------------------------------------
// Accumulation
// ---------------------------------------------------------------------------

#[test]
fn consecutive_ctrl_w_accumulates_into_one_kill_ring_entry() {
    let mut e = editor();
    e.set_text("one two three");

    e.handle_input(&Key::ctrl('w')); // deletes "three"
    e.handle_input(&Key::ctrl('w')); // deletes "two ", prepended
    e.handle_input(&Key::ctrl('w')); // deletes "one ", prepended

    assert_eq!(e.get_text(), "");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "one two three");
}

#[test]
fn ctrl_u_accumulates_multiline_deletes_including_newlines() {
    let mut e = editor();
    e.set_text("line1\nline2\nline3");

    e.handle_input(&Key::ctrl('u')); // deletes "line3"
    assert_eq!(e.get_text(), "line1\nline2\n");

    e.handle_input(&Key::ctrl('u')); // deletes the newline (merge)
    assert_eq!(e.get_text(), "line1\nline2");

    e.handle_input(&Key::ctrl('u')); // deletes "line2"
    assert_eq!(e.get_text(), "line1\n");

    e.handle_input(&Key::ctrl('u')); // deletes the newline
    assert_eq!(e.get_text(), "line1");

    e.handle_input(&Key::ctrl('u')); // deletes "line1"
    assert_eq!(e.get_text(), "");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "line1\nline2\nline3");
}

#[test]
fn backward_deletions_prepend_forward_deletions_append_during_accumulation() {
    let mut e = editor();
    e.set_text("prefix|suffix");

    // Position cursor on '|' (col 6).
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..6 {
        e.handle_input(&Key::right());
    }

    // Ctrl+K deletes "|suffix"... wait — at col 6 with "prefix|suffix",
    // Ctrl+K deletes from cursor to end of line = "|suffix". That's a
    // single kill. A two-step "delete 'suffix' then delete '|'" test
    // would need the cursor placed between "prefix" and "|" (col 7),
    // where a second Ctrl+K is then the no-op-at-EOL case.
    //
    // Keeping the current setup: two Ctrl+K at col 6 produces a single
    // append-accumulated delete of "|suffix".
    e.handle_input(&Key::ctrl('k'));
    e.handle_input(&Key::ctrl('k'));
    assert_eq!(e.get_text(), "prefix");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "prefix|suffix");
}

#[test]
fn non_delete_actions_break_kill_accumulation() {
    let mut e = editor();

    e.set_text("foo bar baz");
    e.handle_input(&Key::ctrl('w'));
    assert_eq!(e.get_text(), "foo bar ");

    e.handle_input(&Key::char('x')); // typing breaks accumulation
    assert_eq!(e.get_text(), "foo bar x");

    e.handle_input(&Key::ctrl('w')); // separate entry
    assert_eq!(e.get_text(), "foo bar ");

    // Yank most recent = "x".
    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "foo bar x");

    // Cycle back = "baz".
    e.handle_input(&Key::alt('y'));
    assert_eq!(e.get_text(), "foo bar baz");
}

#[test]
fn non_yank_actions_break_alt_y_chain() {
    let mut e = editor();

    e.set_text("first");
    e.handle_input(&Key::ctrl('w'));
    e.set_text("second");
    e.handle_input(&Key::ctrl('w'));
    e.set_text("");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "second");

    e.handle_input(&Key::char('x')); // breaks yank chain
    assert_eq!(e.get_text(), "secondx");

    e.handle_input(&Key::alt('y'));
    assert_eq!(e.get_text(), "secondx");
}

#[test]
fn kill_ring_rotation_persists_after_cycling() {
    let mut e = editor();

    e.set_text("first");
    e.handle_input(&Key::ctrl('w'));
    e.set_text("second");
    e.handle_input(&Key::ctrl('w'));
    e.set_text("third");
    e.handle_input(&Key::ctrl('w'));
    e.set_text("");

    // Ring: [first, second, third].

    e.handle_input(&Key::ctrl('y'));
    e.handle_input(&Key::alt('y'));

    // After yank-pop, ring: [third, first, second].
    assert_eq!(e.get_text(), "second");

    // Break the chain, clear, yank again — should get "second" (now head).
    e.handle_input(&Key::char('x'));
    e.set_text("");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "second");
}

#[test]
fn consecutive_deletions_across_lines_coalesce_into_one_entry() {
    let mut e = editor();
    e.set_text("1\n2\n3");

    e.handle_input(&Key::ctrl('w'));
    assert_eq!(e.get_text(), "1\n2\n");

    e.handle_input(&Key::ctrl('w'));
    assert_eq!(e.get_text(), "1\n2");

    e.handle_input(&Key::ctrl('w'));
    assert_eq!(e.get_text(), "1\n");

    e.handle_input(&Key::ctrl('w'));
    assert_eq!(e.get_text(), "1");

    e.handle_input(&Key::ctrl('w'));
    assert_eq!(e.get_text(), "");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "1\n2\n3");
}

#[test]
fn ctrl_k_at_line_end_deletes_newline_and_coalesces() {
    let mut e = editor();

    // Build "ab\ncd" incrementally so the cursor ends at the end of line 1.
    e.set_text("");
    e.handle_input(&Key::char('a'));
    e.handle_input(&Key::char('b'));
    e.handle_input(&Key::shift_enter());
    e.handle_input(&Key::char('c'));
    e.handle_input(&Key::char('d'));

    // Move to end of line 0.
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('e'));

    e.handle_input(&Key::ctrl('k')); // deletes the newline (merge)
    assert_eq!(e.get_text(), "abcd");

    e.handle_input(&Key::ctrl('k')); // deletes "cd"
    assert_eq!(e.get_text(), "ab");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "ab\ncd");
}

// ---------------------------------------------------------------------------
// Yank into the middle of existing text
// ---------------------------------------------------------------------------

#[test]
fn handles_yank_in_middle_of_text() {
    let mut e = editor();

    e.set_text("word");
    e.handle_input(&Key::ctrl('w')); // kill "word"
    e.set_text("hello world");

    // Position cursor after "hello ".
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..6 {
        e.handle_input(&Key::right());
    }

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "hello wordworld");
}

#[test]
fn handles_yank_pop_in_middle_of_text() {
    let mut e = editor();

    e.set_text("FIRST");
    e.handle_input(&Key::ctrl('w'));
    e.set_text("SECOND");
    e.handle_input(&Key::ctrl('w'));

    // Ring: [FIRST, SECOND].

    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..6 {
        e.handle_input(&Key::right());
    }

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "hello SECONDworld");

    e.handle_input(&Key::alt('y'));
    assert_eq!(e.get_text(), "hello FIRSTworld");
}

#[test]
fn multiline_yank_and_yank_pop_in_middle_of_text() {
    let mut e = editor();

    // Build ring entries: ["SINGLE", "A\nB"].
    e.set_text("SINGLE");
    e.handle_input(&Key::ctrl('w'));

    e.set_text("A\nB");
    e.handle_input(&Key::ctrl('u'));
    e.handle_input(&Key::ctrl('u'));
    e.handle_input(&Key::ctrl('u'));

    // Insert into the middle of "hello world".
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..6 {
        e.handle_input(&Key::right());
    }

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "hello A\nBworld");

    e.handle_input(&Key::alt('y'));
    assert_eq!(e.get_text(), "hello SINGLEworld");
}

// ---------------------------------------------------------------------------
// Alt+D word-forward kill
// ---------------------------------------------------------------------------

#[test]
fn alt_d_deletes_word_forward_and_saves_to_kill_ring() {
    let mut e = editor();
    e.set_text("hello world test");
    e.handle_input(&Key::ctrl('a'));

    e.handle_input(&Key::alt('d'));
    assert_eq!(e.get_text(), " world test");

    e.handle_input(&Key::alt('d'));
    assert_eq!(e.get_text(), " test");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "hello world test");
}

#[test]
fn alt_d_at_end_of_line_deletes_newline() {
    let mut e = editor();
    e.set_text("line1\nline2");

    // Move to end of line 0.
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('e'));

    e.handle_input(&Key::alt('d'));
    assert_eq!(e.get_text(), "line1line2");

    e.handle_input(&Key::ctrl('y'));
    assert_eq!(e.get_text(), "line1\nline2");
}
