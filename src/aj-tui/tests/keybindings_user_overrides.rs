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

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::cancellable_loader::CancellableLoader;
use aj_tui::components::editor::Editor;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout};
use aj_tui::components::settings_list::{SettingItem, SettingsList, SettingsListOptions};
use aj_tui::components::text_input::Input;
use aj_tui::keybindings;
use aj_tui::keys::{InputEvent, Key};
use aj_tui::tui::RenderHandle;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;

use support::themes::{default_select_list_theme, identity_settings_list_theme};

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
    let mut editor = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
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

    let mut editor = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
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

// ---------------------------------------------------------------------------
// Per-component rebind-and-fire coverage (PORTING.md H3).
//
// Each test below picks the representative override called out in the
// H3 spec, drives the rebound key through the component, then drives
// the previous default to confirm it no longer fires the action.
// ---------------------------------------------------------------------------

/// `Input` consumes `tui.editor.deleteCharBackward` (default `backspace`).
/// Rebind it to `ctrl+h` and assert the new key deletes one grapheme,
/// while plain `backspace` is now inert and the literal byte remains.
#[test]
#[serial(global_keybindings)]
fn rebinding_tui_editor_delete_char_backward_takes_effect_on_input() {
    keybindings::reset();

    let mut input = Input::new("> ");
    input.set_value("hello");
    // Move cursor to end so a backspace would chop "o" first.
    input.handle_input(&Key::ctrl('e'));

    keybindings::set_user_bindings([("tui.editor.deleteCharBackward", "ctrl+h")]);

    // The new key fires the action.
    let consumed = input.handle_input(&Key::ctrl('h'));
    assert!(
        consumed,
        "ctrl+h must be consumed by Input after the rebind",
    );
    assert_eq!(
        input.value(),
        "hell",
        "ctrl+h should now delete one char backward",
    );

    // The previous default no longer fires `deleteCharBackward`. Plain
    // `backspace` is unbound after the wholesale-replace `set_user_bindings`,
    // so the Input doesn't consume it and the value is unchanged.
    let consumed = input.handle_input(&Key::backspace());
    assert!(
        !consumed,
        "plain backspace must not be consumed once the action is rebound away",
    );
    assert_eq!(
        input.value(),
        "hell",
        "plain backspace must not delete after the rebind",
    );

    keybindings::reset();
}

/// `SelectList` consumes `tui.select.up` (default `up`). Rebind it to
/// `ctrl+p` and assert the new key decrements the selection (with wrap),
/// while plain `up` is now inert and the selection doesn't move.
#[test]
#[serial(global_keybindings)]
fn rebinding_tui_select_up_takes_effect_on_select_list() {
    keybindings::reset();

    let items = vec![
        SelectItem::new("a", "alpha"),
        SelectItem::new("b", "bravo"),
        SelectItem::new("c", "charlie"),
    ];
    let mut list = SelectList::new(
        items,
        5,
        default_select_list_theme(),
        SelectListLayout::default(),
    );
    // Pre-select the second item so `up` has somewhere to move to and we
    // can observe the change without relying on wrap-around.
    list.set_selected_index(1);
    assert_eq!(
        list.selected_item().map(|i| i.value.as_str()),
        Some("b"),
        "precondition: selection starts on 'b'",
    );

    keybindings::set_user_bindings([("tui.select.up", "ctrl+p")]);

    // The new key fires the action — selection moves up to 'a'.
    let consumed = list.handle_input(&Key::ctrl('p'));
    assert!(
        consumed,
        "ctrl+p must be consumed by SelectList after the rebind",
    );
    assert_eq!(
        list.selected_item().map(|i| i.value.as_str()),
        Some("a"),
        "ctrl+p should now move the selection up",
    );

    // The previous default no longer fires `tui.select.up`. Plain `up`
    // is unbound after the wholesale-replace, so the SelectList doesn't
    // consume it and the selection stays put.
    let consumed = list.handle_input(&Key::up());
    assert!(
        !consumed,
        "plain up must not be consumed once the action is rebound away",
    );
    assert_eq!(
        list.selected_item().map(|i| i.value.as_str()),
        Some("a"),
        "plain up must not move the selection after the rebind",
    );

    keybindings::reset();
}

/// `SettingsList` consumes `tui.select.confirm` (default `enter`). Rebind
/// it to `tab` and assert the new key cycles the selected item's value
/// (firing `on_change`), while plain `enter` is now inert.
#[test]
#[serial(global_keybindings)]
fn rebinding_tui_select_confirm_takes_effect_on_settings_list() {
    keybindings::reset();

    let items = vec![SettingItem::cycleable(
        "wrap",
        "Word wrap",
        "off",
        vec!["off".to_string(), "on".to_string()],
    )];

    // Capture the (id, new_value) pairs the manager fires on confirm.
    let changes: Rc<RefCell<Vec<(String, String)>>> = Rc::new(RefCell::new(Vec::new()));
    let captured = Rc::clone(&changes);
    let mut list = SettingsList::new(
        items,
        5,
        identity_settings_list_theme(),
        move |id: &str, val: &str| {
            captured
                .borrow_mut()
                .push((id.to_string(), val.to_string()));
        },
        || {},
        SettingsListOptions::default(),
    );

    keybindings::set_user_bindings([("tui.select.confirm", "tab")]);

    // The new key fires the action — value cycles "off" → "on".
    let consumed = list.handle_input(&Key::tab());
    assert!(
        consumed,
        "tab must be consumed by SettingsList after the rebind",
    );
    assert_eq!(
        changes.borrow().as_slice(),
        &[("wrap".to_string(), "on".to_string())],
        "tab should cycle the selected value via on_change after the rebind",
    );
    assert_eq!(list.value_of("wrap"), Some("on"));

    // The previous default no longer fires `tui.select.confirm`. Plain
    // `enter` is unbound after the wholesale-replace, so SettingsList
    // doesn't consume it and the value doesn't cycle.
    let consumed = list.handle_input(&Key::enter());
    assert!(
        !consumed,
        "plain enter must not be consumed once the action is rebound away",
    );
    assert_eq!(
        changes.borrow().len(),
        1,
        "plain enter must not fire on_change a second time after the rebind",
    );
    assert_eq!(list.value_of("wrap"), Some("on"));

    // Note: Space is hardcoded as a confirm alias regardless of the
    // registry, so plain Space would still cycle the value even after
    // this rebind. That invariant is exercised separately by the
    // Phase D `tests/settings_list.rs` suite — keeping this test
    // narrowly focused on the registry-driven rebind shape.

    keybindings::reset();
}

/// `CancellableLoader` consumes `tui.select.cancel` (defaults `escape`,
/// `ctrl+c`). Rebind it to `ctrl+g` and assert the new key fires
/// `on_abort` and cancels the token, while neither default fires it
/// after the rebind.
#[test]
#[serial(global_keybindings)]
fn rebinding_tui_select_cancel_takes_effect_on_cancellable_loader() {
    keybindings::reset();

    let abort_count = Rc::new(Cell::new(0u32));
    let captured = Rc::clone(&abort_count);
    let mut loader = CancellableLoader::with_identity_styles(RenderHandle::detached(), "working");
    loader.set_on_abort(Box::new(move || {
        captured.set(captured.get() + 1);
    }));
    let token = loader.cancel_token();
    assert!(
        !token.is_cancelled(),
        "precondition: token starts uncancelled",
    );

    keybindings::set_user_bindings([("tui.select.cancel", "ctrl+g")]);

    // The new key fires the action: on_abort runs, the token cancels.
    let consumed = loader.handle_input(&Key::ctrl('g'));
    assert!(
        consumed,
        "ctrl+g must be consumed by CancellableLoader after the rebind",
    );
    assert_eq!(abort_count.get(), 1, "ctrl+g should fire on_abort");
    assert!(token.is_cancelled(), "ctrl+g should cancel the token");
    assert!(loader.is_aborted());

    // Neither of the previous defaults (`escape`, `ctrl+c`) fires the
    // action after the wholesale-replace rebind. Both must be ignored
    // by the loader and `on_abort` must not run a second time.
    let consumed_esc = loader.handle_input(&Key::escape());
    assert!(
        !consumed_esc,
        "plain escape must not be consumed once cancel is rebound away",
    );
    let consumed_ctrl_c = loader.handle_input(&Key::ctrl('c'));
    assert!(
        !consumed_ctrl_c,
        "ctrl+c must not be consumed once cancel is rebound away",
    );
    assert_eq!(
        abort_count.get(),
        1,
        "neither previous default should re-fire on_abort after the rebind",
    );

    keybindings::reset();
}
