//! Tests for the single-line `TextInput` component's byte-form newline
//! fallbacks.
//!
//! In addition to the registry-bound `tui.input.submit` binding,
//! `TextInput::handle_input` consults the shared
//! [`aj_tui::keys::is_newline_event`] helper (the same recognizer the
//! multi-line `Editor` uses for its newline branch) so a literal LF
//! byte arriving as input still submits.
//!
//! `is_newline_event` recognizes the literal LF byte under both raw
//! and non-raw mode (`KeyCode::Char('\n')` no mods, and
//! `KeyCode::Char('j') + CTRL` — Ctrl+J is ASCII LF 0x0A) plus the
//! Alt+Enter byte sequence `\x1b\r` as `KeyCode::Enter + ALT`. For a
//! `TextInput` (single-line, no newline character ever lives in the
//! value) every byte form `is_newline_event` recognizes maps to
//! "submit", not "insert newline" — there's nowhere for a newline
//! to go.
//!
//! Plain Enter, Shift+Enter, Ctrl+Enter, and other modified Enter
//! events are intentionally excluded from `is_newline_event` so the
//! registry can route user-rebound submit / newLine bindings without
//! interference. Those cases are covered by the keybindings tests.

use std::cell::RefCell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::text_input::TextInput;
use aj_tui::keybindings;
use aj_tui::keys::{InputEvent, Key};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn raw_lf_char() -> InputEvent {
    InputEvent::Key(KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE))
}

fn ctrl_j() -> InputEvent {
    InputEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
}

fn alt_enter() -> InputEvent {
    InputEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
}

/// Build a `TextInput` populated with `value`, plus a shared cell
/// that the `on_submit` callback writes the submitted string into.
fn input_with_submit_capture(value: &str) -> (TextInput, Rc<RefCell<Option<String>>>) {
    let captured: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let captured_clone = Rc::clone(&captured);

    let mut input = TextInput::new("> ");
    input.set_value(value);
    input.on_submit = Some(Box::new(move |v: &str| {
        *captured_clone.borrow_mut() = Some(v.to_string());
    }));

    (input, captured)
}

// ---------------------------------------------------------------------------
// Each byte form recognized by `is_newline_event` submits the value.
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn raw_lf_char_submits_the_current_value() {
    keybindings::reset();
    let (mut input, captured) = input_with_submit_capture("hello");

    input.handle_input(&raw_lf_char());

    assert_eq!(captured.borrow().as_deref(), Some("hello"));
}

#[test]
#[serial(global_keybindings)]
fn ctrl_j_submits_the_current_value() {
    keybindings::reset();
    let (mut input, captured) = input_with_submit_capture("hello");

    input.handle_input(&ctrl_j());

    assert_eq!(captured.borrow().as_deref(), Some("hello"));
}

#[test]
#[serial(global_keybindings)]
fn alt_enter_submits_the_current_value() {
    keybindings::reset();
    let (mut input, captured) = input_with_submit_capture("hello");

    input.handle_input(&alt_enter());

    assert_eq!(captured.borrow().as_deref(), Some("hello"));
}

// ---------------------------------------------------------------------------
// Sanity: plain Enter still submits via the registry path. The fallback
// is additive — it doesn't replace the registry-bound submit.
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn plain_enter_still_submits_via_the_registry() {
    keybindings::reset();
    let (mut input, captured) = input_with_submit_capture("hello");

    input.handle_input(&Key::enter());

    assert_eq!(captured.borrow().as_deref(), Some("hello"));
}

// ---------------------------------------------------------------------------
// Plain `j` is just a printable letter; Ctrl+J is the only `j` form
// the helper treats as raw LF. Without this gate the user couldn't
// type the letter `j` into a `TextInput`.
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn plain_j_inserts_a_literal_letter_and_does_not_submit() {
    keybindings::reset();
    let (mut input, captured) = input_with_submit_capture("");

    input.handle_input(&Key::char('j'));

    assert_eq!(input.value(), "j");
    assert_eq!(captured.borrow().as_deref(), None);
}

// ---------------------------------------------------------------------------
// The fallback must not shadow user-customized bindings on `Enter +
// Ctrl` or other modified Enter events. A user who rebinds
// `tui.input.submit` to `ctrl+enter` should still get a clean submit
// on that key, and the byte-form helper must not eat it.
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn ctrl_enter_routes_through_user_rebound_submit_not_the_fallback() {
    keybindings::reset();
    keybindings::set_user_bindings([("tui.input.submit", "ctrl+enter")]);

    let (mut input, captured) = input_with_submit_capture("hi");

    // Plain Enter should no longer submit (user rebound it away).
    input.handle_input(&Key::enter());
    assert_eq!(captured.borrow().as_deref(), None);

    // Ctrl+Enter must reach the registry-bound submit branch — it's
    // intentionally excluded from `is_newline_event` so a rebind here
    // works.
    input.handle_input(&InputEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::CONTROL,
    )));
    assert_eq!(captured.borrow().as_deref(), Some("hi"));

    keybindings::reset();
}

// ---------------------------------------------------------------------------
// A literal `\n` arriving as a `Paste` event is not a keystroke and
// must not submit. The paste handler strips newlines from pasted
// text, and `handle_input`'s `InputEvent::Paste` arm short-circuits
// to `false` regardless of payload.
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn pasted_newline_does_not_submit() {
    keybindings::reset();
    let (mut input, captured) = input_with_submit_capture("");

    input.handle_input(&InputEvent::Paste("hello\nworld".to_string()));

    // Newlines stripped from the pasted content, no submit fired.
    assert_eq!(input.value(), "helloworld");
    assert_eq!(captured.borrow().as_deref(), None);
}
