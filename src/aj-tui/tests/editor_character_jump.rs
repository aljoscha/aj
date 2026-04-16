//! Tests for the editor's character-jump mode (`Ctrl+]` / `Ctrl+Alt+]`).
//!
//! Entering a jump mode turns the next printable key into a search
//! target. Forward jumps land on the first occurrence strictly after
//! the cursor (walking across line boundaries if needed); backward
//! jumps land on the last occurrence strictly before the cursor. The
//! mode is modal: a second press of the same binding cancels, Escape
//! cancels, and any other key either executes the jump or falls back
//! to normal handling.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keybindings;
use aj_tui::keys::Key;
use aj_tui::tui::RenderHandle;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;

use aj_tui::keys::InputEvent;

fn editor() -> Editor {
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e
}

fn jump_forward() -> InputEvent {
    InputEvent::Key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::CONTROL))
}

fn jump_backward() -> InputEvent {
    InputEvent::Key(KeyEvent::new(
        KeyCode::Char(']'),
        KeyModifiers::CONTROL | KeyModifiers::ALT,
    ))
}

// ---------------------------------------------------------------------------
// Forward jump
// ---------------------------------------------------------------------------

#[test]
fn jumps_forward_to_first_occurrence_of_character_on_same_line() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (0, 0));

    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('o'));

    // First 'o' is at col 4 ("hello"'s fifth char).
    assert_eq!(e.cursor(), (0, 4));
}

#[test]
fn jumps_forward_to_next_occurrence_after_cursor() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    for _ in 0..4 {
        e.handle_input(&Key::right());
    }
    assert_eq!(e.cursor(), (0, 4));

    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('o'));

    // Next 'o' strictly after col 4 is in "world", at col 7.
    assert_eq!(e.cursor(), (0, 7));
}

#[test]
fn jumps_forward_across_multiple_lines() {
    let mut e = editor();
    e.set_text("abc\ndef\nghi");
    e.handle_input(&Key::up());
    e.handle_input(&Key::up());
    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (0, 0));

    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('g'));

    assert_eq!(e.cursor(), (2, 0));
}

// ---------------------------------------------------------------------------
// Backward jump
// ---------------------------------------------------------------------------

#[test]
fn jumps_backward_to_first_occurrence_before_cursor_on_same_line() {
    let mut e = editor();
    e.set_text("hello world");
    assert_eq!(e.cursor(), (0, 11));

    e.handle_input(&jump_backward());
    e.handle_input(&Key::char('o'));

    // Last 'o' strictly before col 11 is in "world", at col 7.
    assert_eq!(e.cursor(), (0, 7));
}

#[test]
fn jumps_backward_across_multiple_lines() {
    let mut e = editor();
    e.set_text("abc\ndef\nghi");
    assert_eq!(e.cursor(), (2, 3));

    e.handle_input(&jump_backward());
    e.handle_input(&Key::char('a'));

    assert_eq!(e.cursor(), (0, 0));
}

// ---------------------------------------------------------------------------
// No-match behavior
// ---------------------------------------------------------------------------

#[test]
fn does_nothing_when_character_is_not_found_forward() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (0, 0));

    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('z'));

    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn does_nothing_when_character_is_not_found_backward() {
    let mut e = editor();
    e.set_text("hello world");
    assert_eq!(e.cursor(), (0, 11));

    e.handle_input(&jump_backward());
    e.handle_input(&Key::char('z'));

    assert_eq!(e.cursor(), (0, 11));
}

#[test]
fn jump_is_case_sensitive() {
    let mut e = editor();
    e.set_text("Hello World");
    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (0, 0));

    // Lowercase 'h' doesn't exist — the only 'h' in the text is 'H'.
    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('h'));
    assert_eq!(e.cursor(), (0, 0));

    // Uppercase 'W' does exist.
    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('W'));
    assert_eq!(e.cursor(), (0, 6));
}

// ---------------------------------------------------------------------------
// Cancelling jump mode
// ---------------------------------------------------------------------------

#[test]
fn cancels_jump_mode_when_ctrl_bracket_is_pressed_again() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (0, 0));

    e.handle_input(&jump_forward()); // enter
    e.handle_input(&jump_forward()); // cancel

    // Now 'o' is a normal insert, not a jump target.
    e.handle_input(&Key::char('o'));
    assert_eq!(e.get_text(), "ohello world");
}

#[test]
fn cancels_jump_mode_on_escape_and_processes_the_escape() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (0, 0));

    e.handle_input(&jump_forward());
    e.handle_input(&Key::escape());

    assert_eq!(e.cursor(), (0, 0));

    // Next 'o' is a normal insert.
    e.handle_input(&Key::char('o'));
    assert_eq!(e.get_text(), "ohello world");
}

#[test]
fn cancels_backward_jump_mode_when_ctrl_alt_bracket_is_pressed_again() {
    let mut e = editor();
    e.set_text("hello world");
    assert_eq!(e.cursor(), (0, 11));

    e.handle_input(&jump_backward()); // enter
    e.handle_input(&jump_backward()); // cancel

    // Next 'o' is a normal insert at end.
    e.handle_input(&Key::char('o'));
    assert_eq!(e.get_text(), "hello worldo");
}

// ---------------------------------------------------------------------------
// Special characters, empty text, last_action reset
// ---------------------------------------------------------------------------

#[test]
fn searches_for_special_characters() {
    let mut e = editor();
    e.set_text("foo(bar) = baz;");
    e.handle_input(&Key::ctrl('a'));
    assert_eq!(e.cursor(), (0, 0));

    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('('));
    assert_eq!(e.cursor(), (0, 3));

    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('='));
    assert_eq!(e.cursor(), (0, 9));
}

#[test]
fn handles_empty_text_gracefully() {
    let mut e = editor();
    e.set_text("");
    assert_eq!(e.cursor(), (0, 0));

    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('x'));

    assert_eq!(e.cursor(), (0, 0));
}

#[test]
fn jumping_resets_last_action_so_following_type_starts_new_undo_unit() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));

    // Typing sets last_action = TypeWord.
    e.handle_input(&Key::char('x'));
    assert_eq!(e.get_text(), "xhello world");

    // Jump.
    e.handle_input(&jump_forward());
    e.handle_input(&Key::char('o'));

    // Subsequent type starts a new undo unit because the jump reset
    // last_action. Undo should only rewind 'Y', not the earlier 'x'.
    e.handle_input(&Key::char('Y'));
    assert_eq!(e.get_text(), "xhellYo world");

    e.handle_input(&Key::ctrl('-'));
    assert_eq!(e.get_text(), "xhello world");
}

// ---------------------------------------------------------------------------
// Pi-parity: jump-mode falls through control characters silently (H1)
// ---------------------------------------------------------------------------

/// H1 regression. Pi-tui's jump-mode (editor.ts:534-556) has no
/// explicit `tui.select.cancel` match — only the same-hotkey
/// cancel, the printable-jump arm, and a silent fallthrough that
/// clears `jumpMode` for any control character. The aj-tui port
/// previously added an explicit `KeyCode::Esc` cancel that
/// consumed the key (returned `true`). That diverged from pi by
/// swallowing Escape that the parent surface would otherwise
/// handle. Removed for parity: Esc now silently clears jump_mode
/// and falls through to normal handling, returning `false` so the
/// parent sees the key.
#[test]
fn jump_mode_does_not_consume_escape_for_pi_parity() {
    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));

    e.handle_input(&jump_forward());
    let consumed = e.handle_input(&Key::escape());
    assert!(
        !consumed,
        "Escape in jump-mode must not be consumed (pi-parity); it \
         silently clears jump_mode and falls through so the parent \
         can handle Esc",
    );

    // jump_mode was cleared by the silent fallthrough — the next
    // printable inserts as text rather than acting as a jump target.
    e.handle_input(&Key::char('o'));
    assert_eq!(e.get_text(), "ohello world");
}

/// H1 regression complement. Pi-tui's jump-mode does not consult
/// `tui.select.cancel`, so a user who rebinds it (e.g. to
/// `ctrl+g`) sees the same fallthrough behavior — the rebound key
/// is not specially recognized inside jump-mode; it silently
/// clears the mode and bubbles up. This locks in the pi-parity
/// shape: every other aj-tui modal routes cancel through the
/// registry, but jump-mode is the deliberate exception.
#[test]
#[serial(global_keybindings)]
fn jump_mode_does_not_consume_user_bound_cancel_for_pi_parity() {
    keybindings::reset();
    keybindings::set_user_bindings([("tui.select.cancel", "ctrl+g")]);

    let mut e = editor();
    e.set_text("hello world");
    e.handle_input(&Key::ctrl('a'));

    e.handle_input(&jump_forward());
    let consumed = e.handle_input(&Key::ctrl('g'));
    assert!(
        !consumed,
        "Ctrl+G with cancel rebound must not be explicitly consumed \
         by jump-mode (pi-parity); jump-mode does not consult \
         tui.select.cancel",
    );

    // Subsequent 'o' inserts as text — the silent fallthrough
    // already cleared jump_mode.
    e.handle_input(&Key::char('o'));
    assert_eq!(e.get_text(), "ohello world");

    keybindings::reset();
}
