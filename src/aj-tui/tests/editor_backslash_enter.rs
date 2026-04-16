//! Tests for the Editor backslash+Enter newline workaround.
//!
//! Terminals can't always distinguish `Enter` from `Shift+Enter` (absent
//! Kitty keyboard protocol), so the convention is to let users type `\`
//! immediately before `Enter` to insert a literal newline without
//! submitting. This file covers the full semantics of that workaround.
//!
//! It also covers the *inverse* workaround that fires in the "swap
//! config" — the user has bound `shift+enter` to `tui.input.submit` (and
//! typically `enter` to `tui.input.newLine`). In that config, plain
//! `\<Enter>` should *submit* instead of inserting a newline. Mirrors
//! the original framework's `shouldSubmitOnBackslashEnter`.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keybindings;
use aj_tui::keys::Key;
use aj_tui::tui::RenderHandle;
use serial_test::serial;

fn editor() -> Editor {
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.set_focused(true);
    e
}

#[test]
#[serial(global_keybindings)]
fn backslash_is_inserted_immediately_without_buffering() {
    keybindings::reset();
    let mut e = editor();
    e.handle_input(&Key::char('\\'));
    // The backslash is visible right away — the workaround does not buffer.
    assert_eq!(e.get_text(), "\\");
}

#[test]
#[serial(global_keybindings)]
fn standalone_backslash_followed_by_enter_inserts_a_newline() {
    keybindings::reset();
    let mut e = editor();
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::enter());
    // The backslash is consumed; a newline takes its place.
    assert_eq!(e.get_text(), "\n");
}

#[test]
#[serial(global_keybindings)]
fn backslash_followed_by_other_characters_is_inserted_normally() {
    keybindings::reset();
    let mut e = editor();
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::char('x'));
    assert_eq!(e.get_text(), "\\x");
}

#[test]
#[serial(global_keybindings)]
fn enter_with_backslash_not_immediately_before_cursor_submits_normally() {
    keybindings::reset();
    let submitted = Rc::new(RefCell::new(false));
    let flag = Rc::clone(&submitted);

    let mut e = editor();
    e.on_submit = Some(Box::new(move |_text: &str| {
        *flag.borrow_mut() = true;
    }));

    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::char('x'));
    e.handle_input(&Key::enter());

    // Cursor was preceded by `x`, not `\`, so the submit path fires.
    assert!(
        *submitted.borrow(),
        "on_submit should have fired since the cursor was not immediately after a backslash",
    );
}

#[test]
#[serial(global_keybindings)]
fn enter_removes_only_the_single_trailing_backslash() {
    keybindings::reset();
    let mut e = editor();
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::char('\\'));
    assert_eq!(e.get_text(), "\\\\\\");

    e.handle_input(&Key::enter());
    // Only the final backslash gets converted into the newline.
    assert_eq!(e.get_text(), "\\\\\n");
}

// --- Inverse workaround: `\<Enter>` submits in the "swap config" ---
//
// Default keymap: `enter` → submit, `shift+enter` → newLine. In the
// "swap config", the user has rebound:
//   - `tui.input.submit` to include `shift+enter` (so Shift+Enter
//     submits), and
//   - `tui.input.newLine` to include `enter` (so plain Enter inserts a
//     newline).
//
// In that config, plain Enter is a newline by default. `\<Enter>` is
// the escape hatch to submit anyway.

#[test]
#[serial(global_keybindings)]
fn enter_with_backslash_in_swap_config_submits_instead_of_newline() {
    keybindings::reset();

    let submitted: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let captured = Rc::clone(&submitted);

    let mut e = editor();
    e.on_submit = Some(Box::new(move |text: &str| {
        *captured.borrow_mut() = Some(text.to_string());
    }));

    // Swap config: enter → newLine, shift+enter → submit.
    keybindings::set_user_bindings([
        ("tui.input.newLine", vec!["enter"]),
        ("tui.input.submit", vec!["shift+enter"]),
    ]);

    e.handle_input(&Key::char('h'));
    e.handle_input(&Key::char('i'));
    e.handle_input(&Key::char('\\'));
    // Plain Enter, with cursor preceded by `\`. Inverse gate fires:
    // strip the backslash and submit. Without the inverse workaround
    // this would have inserted a newline (because plain Enter is now
    // newLine in this config).
    e.handle_input(&Key::enter());

    assert_eq!(
        submitted.borrow().clone(),
        Some("hi".to_string()),
        "\\<Enter> should submit (with the trailing backslash stripped) \
         in the swap config",
    );
    assert_eq!(
        e.get_text(),
        "",
        "the editor should reset after the inverse workaround submits",
    );

    keybindings::reset();
}

#[test]
#[serial(global_keybindings)]
fn enter_with_backslash_in_default_config_still_inserts_newline() {
    keybindings::reset();

    let submitted: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let captured = Rc::clone(&submitted);

    let mut e = editor();
    e.on_submit = Some(Box::new(move |text: &str| {
        *captured.borrow_mut() = Some(text.to_string());
    }));

    // Default keymap: enter → submit, shift+enter → newLine. The
    // submit-branch workaround applies (\<Enter> → newline), the
    // inverse gate must NOT fire because shift+enter is not a submit
    // key.
    e.handle_input(&Key::char('h'));
    e.handle_input(&Key::char('i'));
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::enter());

    assert_eq!(
        submitted.borrow().clone(),
        None,
        "default config: \\<Enter> must not submit",
    );
    assert_eq!(
        e.get_text(),
        "hi\n",
        "default config: \\<Enter> inserts a newline (standard workaround)",
    );

    keybindings::reset();
}

#[test]
#[serial(global_keybindings)]
fn shift_enter_with_backslash_in_swap_config_submits_normally_no_strip() {
    // The inverse gate is keyed off of *plain* `enter` only — pressing
    // Shift+Enter (which is a submit key in the swap config) takes the
    // submit branch, where the standard workaround would mis-fire and
    // insert a newline. We assert here that the backslash is stripped
    // and a newline is inserted (the standard `submit-branch` workaround
    // — same as in the default config), since Shift+Enter goes through
    // `tui.input.submit` matching, not the newline branch.
    //
    // Actually: in the swap config, `tui.input.newLine` includes
    // `shift+enter` only if the user added it. We bind only
    // shift+enter → submit here, leaving newLine bound to the default
    // shift+enter, which means BOTH match for Shift+Enter. The newLine
    // branch is checked first, so Shift+Enter routes there. Plain
    // `enter` doesn't qualify for the inverse gate (matchesKey "enter"
    // requires the modifier to be exactly NONE), so the standard
    // workaround does NOT fire either.
    //
    // Net: Shift+Enter with a trailing backslash inserts a newline
    // (the trailing `\` is preserved as part of the line).
    keybindings::reset();

    let submitted: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let captured = Rc::clone(&submitted);

    let mut e = editor();
    e.on_submit = Some(Box::new(move |text: &str| {
        *captured.borrow_mut() = Some(text.to_string());
    }));

    // Swap config: just the submit override; newLine keeps its default
    // (`shift+enter`).
    keybindings::set_user_bindings([("tui.input.submit", vec!["shift+enter"])]);

    e.handle_input(&Key::char('h'));
    e.handle_input(&Key::char('i'));
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::shift_enter());

    assert_eq!(
        submitted.borrow().clone(),
        None,
        "Shift+Enter in swap config takes the newLine branch (default \
         shift+enter binding), and the inverse gate is keyed off plain \
         enter only — submit must not fire",
    );
    assert_eq!(
        e.get_text(),
        "hi\\\n",
        "Shift+Enter inserts a newline; the trailing backslash is \
         preserved",
    );

    keybindings::reset();
}

#[test]
#[serial(global_keybindings)]
fn enter_without_backslash_in_swap_config_inserts_newline_not_submits() {
    // Without the trailing backslash the inverse gate cannot fire
    // (it requires cursor preceded by `\`). Plain Enter routes to the
    // newline branch (because `enter` is bound to `tui.input.newLine`)
    // and inserts a newline.
    keybindings::reset();

    let submitted: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let captured = Rc::clone(&submitted);

    let mut e = editor();
    e.on_submit = Some(Box::new(move |text: &str| {
        *captured.borrow_mut() = Some(text.to_string());
    }));

    keybindings::set_user_bindings([
        ("tui.input.newLine", vec!["enter"]),
        ("tui.input.submit", vec!["shift+enter"]),
    ]);

    e.handle_input(&Key::char('h'));
    e.handle_input(&Key::char('i'));
    e.handle_input(&Key::enter());

    assert_eq!(submitted.borrow().clone(), None, "submit must not fire");
    assert_eq!(e.get_text(), "hi\n", "plain Enter inserts a newline");

    keybindings::reset();
}

#[test]
#[serial(global_keybindings)]
fn disable_submit_blocks_inverse_workaround_too() {
    // `disable_submit = true` short-circuits both the standard
    // workaround and the inverse gate — pressing `\<Enter>` should
    // simply insert a newline (with the backslash preserved), never
    // submit.
    keybindings::reset();

    let submitted: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let captured = Rc::clone(&submitted);

    let mut e = editor();
    e.disable_submit = true;
    e.on_submit = Some(Box::new(move |text: &str| {
        *captured.borrow_mut() = Some(text.to_string());
    }));

    keybindings::set_user_bindings([
        ("tui.input.newLine", vec!["enter"]),
        ("tui.input.submit", vec!["shift+enter"]),
    ]);

    e.handle_input(&Key::char('h'));
    e.handle_input(&Key::char('i'));
    e.handle_input(&Key::char('\\'));
    e.handle_input(&Key::enter());

    assert_eq!(
        submitted.borrow().clone(),
        None,
        "disable_submit blocks the inverse workaround's submit",
    );
    assert_eq!(
        e.get_text(),
        "hi\\\n",
        "the backslash is preserved and Enter inserts a newline",
    );

    keybindings::reset();
}
