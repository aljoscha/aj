//! Tests for the Editor's byte-form newline fallbacks.
//!
//! Pi-tui's editor newline branch fires not just for the registry-
//! bound `tui.input.newLine` keystroke, but also for a set of raw
//! byte forms a terminal might deliver an Enter-like keystroke as
//! (`\x1b\r`, raw `\n`, `\x1b[13;2~`, …). These fallbacks fire
//! **regardless of `disableSubmit`** so a terminal whose Shift+Enter
//! encoding bypasses the registry still produces a newline.
//!
//! The Rust port replicates this with the
//! [`aj_tui::keys::is_newline_event`] helper. Crossterm 0.28 maps the
//! various encodings to:
//!
//! - `\x1b\r` → `KeyCode::Enter + ALT` (Alt+Enter)
//! - raw `\n` (in raw mode) → `KeyCode::Char('j') + CONTROL`
//!   (Ctrl+J — historically `Ctrl+J == LF`)
//! - raw `\n` (in non-raw mode / piped input) → `KeyCode::Char('\n')`
//!
//! `\x1b[13;2~` is not parsed by crossterm 0.28 and is currently
//! lost before reaching us; that's a known gap documented on
//! `is_newline_event`.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keybindings;
use aj_tui::keys::{InputEvent, Key};
use aj_tui::tui::RenderHandle;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serial_test::serial;

fn raw_lf_char() -> InputEvent {
    InputEvent::Key(KeyEvent::new(KeyCode::Char('\n'), KeyModifiers::NONE))
}

fn ctrl_j() -> InputEvent {
    InputEvent::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
}

fn alt_enter() -> InputEvent {
    InputEvent::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT))
}

// ---------------------------------------------------------------------------
// disable_submit = true (matches existing F17 tests)
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn raw_lf_inserts_a_newline_in_disable_submit_mode() {
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e.handle_input(&Key::char('a'));
    e.handle_input(&raw_lf_char());
    e.handle_input(&Key::char('b'));
    assert_eq!(e.get_text(), "a\nb");
}

#[test]
#[serial(global_keybindings)]
fn alt_enter_inserts_a_newline_in_disable_submit_mode() {
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e.handle_input(&Key::char('a'));
    e.handle_input(&alt_enter());
    e.handle_input(&Key::char('b'));
    assert_eq!(e.get_text(), "a\nb");
}

#[test]
#[serial(global_keybindings)]
fn ctrl_j_inserts_a_newline_in_disable_submit_mode() {
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e.handle_input(&Key::char('a'));
    e.handle_input(&ctrl_j());
    e.handle_input(&Key::char('b'));
    assert_eq!(e.get_text(), "a\nb");
}

// ---------------------------------------------------------------------------
// disable_submit = false: pi-tui fires the byte fallbacks regardless of
// `disableSubmit`, so the same byte forms must insert a newline here too.
// Plain Enter still submits (the fallback excludes plain `KeyCode::Enter`).
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn raw_lf_inserts_a_newline_in_normal_mode_too() {
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.set_focused(true);
    e.handle_input(&Key::char('a'));
    e.handle_input(&raw_lf_char());
    e.handle_input(&Key::char('b'));
    // Newline split occurred; no submit fired.
    assert_eq!(e.get_text(), "a\nb");
    assert_eq!(e.take_submitted(), None);
}

#[test]
#[serial(global_keybindings)]
fn alt_enter_inserts_a_newline_in_normal_mode_too() {
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.set_focused(true);
    e.handle_input(&Key::char('a'));
    e.handle_input(&alt_enter());
    e.handle_input(&Key::char('b'));
    assert_eq!(e.get_text(), "a\nb");
    assert_eq!(e.take_submitted(), None);
}

#[test]
#[serial(global_keybindings)]
fn ctrl_j_inserts_a_newline_in_normal_mode_too() {
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.set_focused(true);
    e.handle_input(&Key::char('a'));
    e.handle_input(&ctrl_j());
    e.handle_input(&Key::char('b'));
    assert_eq!(e.get_text(), "a\nb");
    assert_eq!(e.take_submitted(), None);
}

#[test]
#[serial(global_keybindings)]
fn plain_enter_in_normal_mode_still_submits() {
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.set_focused(true);
    e.handle_input(&Key::char('h'));
    e.handle_input(&Key::char('i'));
    // Plain `KeyCode::Enter` (no modifiers) is excluded from the
    // helper precisely so the submit branch can still fire on it.
    e.handle_input(&Key::enter());
    assert_eq!(e.take_submitted(), Some("hi".to_string()));
}

// ---------------------------------------------------------------------------
// Sanity: the helper is precise — non-newline keys still behave as before.
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn plain_j_inserts_a_literal_letter_not_a_newline() {
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    // Only Ctrl+J is treated as raw LF; plain `j` is ordinary text.
    e.handle_input(&InputEvent::Key(KeyEvent::new(
        KeyCode::Char('j'),
        KeyModifiers::NONE,
    )));
    assert_eq!(e.get_text(), "j");
}

// ---------------------------------------------------------------------------
// The fallback must not shadow user-customized bindings on
// `Enter + Ctrl` (someone rebinds submit) or `Enter + Shift` (the default
// `tui.input.newLine` already handles this via the registry, and a user
// might rebind it to something else).
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn ctrl_enter_is_not_caught_by_the_fallback_so_user_can_rebind_submit_to_it() {
    keybindings::reset();
    keybindings::set_user_bindings([("tui.input.submit", "ctrl+enter")]);
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.set_focused(true);
    e.handle_input(&Key::char('h'));
    e.handle_input(&Key::char('i'));
    // Ctrl+Enter must reach the submit branch, not get eaten by the
    // newline fallback.
    e.handle_input(&InputEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::CONTROL,
    )));
    assert_eq!(e.take_submitted(), Some("hi".to_string()));
    keybindings::reset();
}

#[test]
#[serial(global_keybindings)]
fn shift_enter_still_inserts_a_newline_via_the_registry() {
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.set_focused(true);
    // Default `tui.input.newLine = shift+enter`; the registry catches
    // this *before* the byte-form fallback. Verifying that path still
    // works and isn't accidentally broken by helper changes.
    e.handle_input(&Key::char('a'));
    e.handle_input(&Key::shift_enter());
    e.handle_input(&Key::char('b'));
    assert_eq!(e.get_text(), "a\nb");
    assert_eq!(e.take_submitted(), None);
}

// ---------------------------------------------------------------------------
// H2: pi parity for `disable_submit` + submit-key. Pi-tui's submit
// branch (`editor.ts:735-749`) tests `disableSubmit` *inside* the
// branch and returns silently \u2014 no "submit-key becomes newline"
// fallback. We mirror that exactly: the submit key is consumed (the
// editor returns `true`) without inserting a newline, falling
// through, or otherwise editing the buffer. Newlines come from
// `tui.input.newLine` (Shift+Enter by default) or the byte-form
// fallbacks.
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn rebound_submit_is_silently_consumed_in_disable_submit_mode() {
    // User has rebound submit to ctrl+enter; with `disable_submit = true`
    // the rebound submit key is consumed silently (matching pi-tui's
    // bare `return;` in the submit branch).
    keybindings::reset();
    keybindings::set_user_bindings([("tui.input.submit", "ctrl+enter")]);
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e.handle_input(&Key::char('a'));
    let consumed = e.handle_input(&InputEvent::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::CONTROL,
    )));
    e.handle_input(&Key::char('b'));
    // Submit key was consumed (returned `true`) but did nothing else;
    // the buffer is just "ab" with no newline.
    assert!(consumed, "submit key should be consumed");
    assert_eq!(e.get_text(), "ab");
    assert_eq!(e.take_submitted(), None);
    keybindings::reset();
}

#[test]
#[serial(global_keybindings)]
fn default_submit_is_silently_consumed_in_disable_submit_mode() {
    // With the default submit binding (`enter`), plain Enter under
    // `disable_submit = true` is a silent no-op (pi parity). Newlines
    // come from Shift+Enter via `tui.input.newLine`, not from the
    // submit key.
    keybindings::reset();
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e.handle_input(&Key::char('a'));
    let consumed = e.handle_input(&Key::enter());
    e.handle_input(&Key::char('b'));
    assert!(consumed, "submit key should be consumed");
    assert_eq!(e.get_text(), "ab");
    assert_eq!(e.take_submitted(), None);
    // Shift+Enter does insert a newline via `tui.input.newLine`.
    e.handle_input(&Key::shift_enter());
    e.handle_input(&Key::char('c'));
    assert_eq!(e.get_text(), "ab\nc");
}

#[test]
#[serial(global_keybindings)]
fn plain_enter_falls_through_when_submit_is_rebound_away_in_disable_submit_mode() {
    // With submit rebound to ctrl+enter and `disable_submit = true`,
    // plain Enter doesn't match `tui.input.submit`, doesn't match
    // `tui.input.newLine` (still `shift+enter`), and isn't a byte
    // fallback. It falls all the way through unhandled \u2014 the
    // editor returns `false` and the parent surface can handle Esc/
    // Enter etc. itself.
    keybindings::reset();
    keybindings::set_user_bindings([("tui.input.submit", "ctrl+enter")]);
    let mut e = Editor::new(
        RenderHandle::detached(),
        support::themes::default_editor_theme(),
    );
    e.disable_submit = true;
    e.set_focused(true);
    e.handle_input(&Key::char('a'));
    let consumed = e.handle_input(&Key::enter());
    e.handle_input(&Key::char('b'));
    assert!(
        !consumed,
        "plain Enter should fall through unhandled when submit is rebound away",
    );
    assert_eq!(e.get_text(), "ab");
    assert_eq!(e.take_submitted(), None);
    keybindings::reset();
}
