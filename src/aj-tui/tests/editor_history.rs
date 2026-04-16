//! Tests for `Editor` prompt-history navigation.
//!
//! This file is a self-contained slice: no filesystem, no autocomplete,
//! no virtual terminal — the tests drive the component through
//! `handle_input` / `add_to_history` / `set_text` and assert on
//! `get_text()`.
//!
//! The other editor sub-suites (word wrapping, kill ring, undo,
//! autocomplete, ...) live in their own files so each batch stays
//! reviewable.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::{InputEvent, Key};
use aj_tui::tui::RenderHandle;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn editor() -> Editor {
    // `disable_submit = true` prevents the default Enter-as-submit path
    // from firing; irrelevant to history tests but keeps behavior
    // consistent if a test happens to send Enter.
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e
}

fn send(e: &mut Editor, event: InputEvent) {
    e.handle_input(&event);
}

fn up(e: &mut Editor) {
    send(e, Key::up());
}

fn down(e: &mut Editor) {
    send(e, Key::down());
}

fn type_char(e: &mut Editor, c: char) {
    send(e, Key::char(c));
}

// ---------------------------------------------------------------------------
// Navigation basics
// ---------------------------------------------------------------------------

#[test]
fn up_does_nothing_when_history_is_empty() {
    let mut e = editor();
    up(&mut e);
    assert_eq!(e.get_text(), "");
}

#[test]
fn up_shows_most_recent_entry_when_editor_is_empty() {
    let mut e = editor();
    e.add_to_history("first prompt");
    e.add_to_history("second prompt");
    up(&mut e);
    assert_eq!(e.get_text(), "second prompt");
}

#[test]
fn repeated_up_cycles_through_entries_oldest_sticks() {
    let mut e = editor();
    e.add_to_history("first");
    e.add_to_history("second");
    e.add_to_history("third");

    up(&mut e);
    assert_eq!(e.get_text(), "third");
    up(&mut e);
    assert_eq!(e.get_text(), "second");
    up(&mut e);
    assert_eq!(e.get_text(), "first");
    // Additional Up stays at the oldest entry.
    up(&mut e);
    assert_eq!(e.get_text(), "first");
}

#[test]
fn down_returns_to_empty_editor_after_browsing() {
    let mut e = editor();
    e.add_to_history("prompt");
    up(&mut e);
    assert_eq!(e.get_text(), "prompt");
    down(&mut e);
    assert_eq!(e.get_text(), "");
}

#[test]
fn down_walks_forward_through_history_and_exits_to_empty() {
    let mut e = editor();
    e.add_to_history("first");
    e.add_to_history("second");
    e.add_to_history("third");

    // Walk to the oldest.
    up(&mut e); // third
    up(&mut e); // second
    up(&mut e); // first

    // Then walk back.
    down(&mut e);
    assert_eq!(e.get_text(), "second");
    down(&mut e);
    assert_eq!(e.get_text(), "third");
    down(&mut e);
    assert_eq!(e.get_text(), "");
}

// ---------------------------------------------------------------------------
// Exit conditions
// ---------------------------------------------------------------------------

#[test]
fn typing_a_character_exits_history_mode_in_place() {
    let mut e = editor();
    e.add_to_history("old prompt");
    up(&mut e);
    type_char(&mut e, 'x');
    assert_eq!(e.get_text(), "old promptx");
}

#[test]
fn set_text_exits_history_mode_and_next_up_starts_from_most_recent() {
    let mut e = editor();
    e.add_to_history("first");
    e.add_to_history("second");

    up(&mut e);
    assert_eq!(e.get_text(), "second");

    // External clear; should drop the in-flight history browsing state.
    e.set_text("");

    // Fresh browsing resumes from the most recent entry.
    up(&mut e);
    assert_eq!(e.get_text(), "second");
}

// ---------------------------------------------------------------------------
// Ring maintenance
// ---------------------------------------------------------------------------

#[test]
fn empty_and_whitespace_only_entries_are_not_added() {
    let mut e = editor();
    e.add_to_history("");
    e.add_to_history("   ");
    e.add_to_history("valid");

    up(&mut e);
    assert_eq!(e.get_text(), "valid");

    // Only one entry exists; another Up stays on it.
    up(&mut e);
    assert_eq!(e.get_text(), "valid");
}

#[test]
fn consecutive_duplicates_collapse_to_a_single_entry() {
    let mut e = editor();
    e.add_to_history("same");
    e.add_to_history("same");
    e.add_to_history("same");

    up(&mut e);
    assert_eq!(e.get_text(), "same");
    up(&mut e);
    assert_eq!(e.get_text(), "same");
}

#[test]
fn non_consecutive_duplicates_are_kept() {
    let mut e = editor();
    e.add_to_history("first");
    e.add_to_history("second");
    e.add_to_history("first"); // not adjacent to the earlier "first"

    up(&mut e);
    assert_eq!(e.get_text(), "first");
    up(&mut e);
    assert_eq!(e.get_text(), "second");
    up(&mut e);
    assert_eq!(e.get_text(), "first");
}

#[test]
fn up_does_cursor_movement_when_the_editor_already_has_content() {
    let mut e = editor();
    e.add_to_history("history item");
    e.set_text("line1\nline2");

    // After set_text the cursor is at end of line2. Up should move the
    // cursor to line1 (cursor movement), not replace the buffer with the
    // history entry.
    up(&mut e);

    // Confirm by typing: the character should land inside line1 at the
    // column where the cursor happened to be (end-of-line in our
    // implementation).
    type_char(&mut e, 'X');
    assert_eq!(e.get_text(), "line1X\nline2");
}

#[test]
fn history_is_capped_at_the_documented_limit() {
    let mut e = editor();
    let limit = Editor::HISTORY_LIMIT;

    // Push five more than the cap.
    for i in 0..(limit + 5) {
        e.add_to_history(&format!("prompt {i}"));
    }

    // Walk back through the entire remaining history.
    for _ in 0..limit {
        up(&mut e);
    }

    // Oldest surviving entry is `prompt 5` (`prompt 0..=4` fell off).
    assert_eq!(e.get_text(), "prompt 5");

    // Additional Up stays on the oldest entry.
    up(&mut e);
    assert_eq!(e.get_text(), "prompt 5");
}

// ---------------------------------------------------------------------------
// Multi-line entries: Up/Down should walk the cursor within the entry
// before navigating to an adjacent history item.
// ---------------------------------------------------------------------------

#[test]
fn down_on_last_line_of_multi_line_entry_exits_history() {
    let mut e = editor();
    e.add_to_history("line1\nline2\nline3");

    up(&mut e);
    assert_eq!(e.get_text(), "line1\nline2\nline3");

    // Cursor is on the last line; Down exits history, returning the
    // editor to its pre-browsing (empty) state.
    down(&mut e);
    assert_eq!(e.get_text(), "");
}

#[test]
fn up_walks_cursor_through_multi_line_entry_before_older_history() {
    let mut e = editor();
    e.add_to_history("older entry");
    e.add_to_history("line1\nline2\nline3");

    up(&mut e);
    assert_eq!(e.get_text(), "line1\nline2\nline3");

    // Still the same entry: Up only moves the cursor while it isn't on
    // the first visual line.
    up(&mut e);
    assert_eq!(e.get_text(), "line1\nline2\nline3");
    up(&mut e);
    assert_eq!(e.get_text(), "line1\nline2\nline3");

    // Now the cursor is on the first line, so Up navigates.
    up(&mut e);
    assert_eq!(e.get_text(), "older entry");
}

#[test]
fn down_walks_cursor_through_multi_line_entry_before_exiting_history() {
    let mut e = editor();
    e.add_to_history("line1\nline2\nline3");

    // Step: Up shows the entry; repeated Up moves cursor up.
    up(&mut e); // show entry, cursor on last line
    up(&mut e); // cursor on line2
    up(&mut e); // cursor on line1

    // Down should now walk the cursor back down within the same entry.
    down(&mut e);
    assert_eq!(e.get_text(), "line1\nline2\nline3");
    down(&mut e);
    assert_eq!(e.get_text(), "line1\nline2\nline3");

    // Cursor now on last line; Down exits history.
    down(&mut e);
    assert_eq!(e.get_text(), "");
}

#[test]
fn down_after_mid_entry_cursor_movement_transitions_to_newer_entry() {
    // When history contains multiple entries and the user has walked
    // back past the newest (older entry is showing), Down should take
    // them forward to the newer entry only after the cursor has
    // reached the last line of the currently-displayed entry. The
    // sibling `down_walks_cursor_through_multi_line_entry_before_exiting_history`
    // test covers exit-to-empty; this one covers the intermediate
    // forward-to-newer transition.
    let mut e = editor();
    e.add_to_history("older"); // older entry (single line)
    e.add_to_history("newA\nnewB\nnewC"); // newer entry (multi-line)

    up(&mut e); // show newest entry; cursor lands on "newC" (last line)
    up(&mut e); // cursor walks up to "newB"
    up(&mut e); // cursor walks up to "newA"
    up(&mut e); // transitions to older entry ("older")
    assert_eq!(e.get_text(), "older");

    // Now Down from the older (single-line) entry should go directly
    // back to the newer entry. `history_down` resets the cursor to
    // the last line of the new entry, so the next Down immediately
    // exits history (cursor is on the last visual line already).
    down(&mut e);
    assert_eq!(
        e.get_text(),
        "newA\nnewB\nnewC",
        "Down from older entry should transition forward to the newer \
         multi-line entry",
    );
    let (row, _) = e.cursor();
    assert_eq!(
        row, 2,
        "transitioning forward to a multi-line entry lands the cursor \
         on its last line",
    );
    down(&mut e);
    assert_eq!(e.get_text(), "", "final Down exits history to the draft");
}
