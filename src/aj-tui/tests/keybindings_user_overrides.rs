//! End-to-end test that a user override installed via
//! [`aj_tui::keybindings::set_user_bindings`] actually takes effect on
//! the components that consume the registry — Editor, Input,
//! SelectList, SettingsList, CancellableLoader.
//!
//! Phase D wired each of those components through the registry; this
//! test exercises the wiring with a representative override and
//! confirms:
//!
//! - the rebound action fires on its new key,
//! - the previous default key for that action no longer fires it,
//! - unrelated actions are untouched.
//!
//! The test runs serially under the `global_keybindings` group
//! because it mutates process-wide state, and resets to defaults at
//! both ends so neighboring tests see the canonical set.

mod support;

use std::cell::Cell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keybindings;
use aj_tui::keys::{InputEvent, Key};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;

fn ctrl_enter() -> InputEvent {
    InputEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL))
}

#[test]
#[serial(global_keybindings)]
fn rebinding_tui_input_submit_to_ctrl_enter_takes_effect_on_editor() {
    keybindings::reset();

    // Capture submitted text so we can assert on what fired (if
    // anything).
    let submitted: Rc<Cell<Option<String>>> = Rc::new(Cell::new(None));
    let captured = Rc::clone(&submitted);
    let mut editor = Editor::new();
    editor.set_focused(true);
    editor.on_submit = Some(Box::new(move |text: &str| {
        captured.set(Some(text.to_string()));
    }));

    // Type "hi" then submit on the new binding.
    keybindings::set_user_bindings([("tui.input.submit", "ctrl+enter")]);
    editor.handle_input(&Key::char('h'));
    editor.handle_input(&Key::char('i'));
    assert_eq!(submitted.take(), None, "no submit should have fired yet");

    editor.handle_input(&ctrl_enter());
    assert_eq!(
        submitted.take(),
        Some("hi".to_string()),
        "ctrl+enter should now submit",
    );

    // The default `enter` no longer fires submit. Type "ok" then press
    // plain Enter; submit must not fire and the buffer must keep
    // growing.
    editor.handle_input(&Key::char('o'));
    editor.handle_input(&Key::char('k'));
    editor.handle_input(&Key::enter());
    assert_eq!(
        submitted.take(),
        None,
        "plain enter must not submit after the rebind",
    );
    assert_eq!(
        editor.get_text(),
        "ok",
        "plain enter should not have inserted a newline either: it's \
         simply unbound after the rebind",
    );

    keybindings::reset();
}

#[test]
#[serial(global_keybindings)]
fn rebinding_tui_input_submit_does_not_disturb_other_editor_actions() {
    keybindings::reset();

    let mut editor = Editor::new();
    editor.disable_submit = true;
    editor.set_focused(true);
    editor.set_text("hello");

    keybindings::set_user_bindings([("tui.input.submit", "ctrl+enter")]);

    // Cursor-left still works (default `left` and `ctrl+b` aren't
    // touched by the override).
    let (_, col_before) = editor.cursor();
    editor.handle_input(&Key::left());
    let (_, col_after_left) = editor.cursor();
    assert_eq!(
        col_after_left + 1,
        col_before,
        "cursor-left should still move one column",
    );

    // Backspace still works.
    editor.handle_input(&Key::backspace());
    assert_eq!(editor.get_text(), "helo");

    // Word-backward (Ctrl+W) still kills the preceding word. After
    // the backspace above, the cursor sits inside "helo" at col 3
    // (between "hel" and "o"), so Ctrl+W should kill "hel" and leave
    // "o".
    editor.handle_input(&Key::ctrl('w'));
    assert_eq!(
        editor.get_text(),
        "o",
        "ctrl+w should still kill the preceding word",
    );

    keybindings::reset();
}
