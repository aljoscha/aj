//! Tests for the editor's inline `shift+backspace` / `shift+delete`
//! aliases.
//!
//! These are net-new behaviors gained from the Phase D registry
//! refactor: the registry never carried `shift+backspace` /
//! `shift+delete` because the original framework also matches them
//! inline (outside the registry) with `matchesKey(data,
//! "shift+backspace")`. The Rust port mirrors that shape via
//! `key_id_matches(event, "shift+backspace")` alongside the registry
//! check so users on terminals that report
//! `Backspace + SHIFT` get the same delete behavior as plain
//! Backspace.

mod support;

use aj_tui::component::Component;
use aj_tui::components::editor::Editor;
use aj_tui::keys::InputEvent;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn editor_with(text: &str) -> Editor {
    let mut e = Editor::new();
    e.disable_submit = true;
    e.set_focused(true);
    e.set_text(text);
    e
}

fn shift_event(code: KeyCode) -> InputEvent {
    InputEvent::Key(KeyEvent::new(code, KeyModifiers::SHIFT))
}

#[test]
fn shift_backspace_deletes_the_previous_character_like_plain_backspace() {
    let mut e = editor_with("hello");
    e.handle_input(&shift_event(KeyCode::Backspace));
    assert_eq!(e.get_text(), "hell");
}

#[test]
fn shift_delete_deletes_the_next_character_like_plain_delete() {
    let mut e = editor_with("hello");
    // Move cursor to the start of the line.
    e.handle_input(&InputEvent::Key(KeyEvent::new(
        KeyCode::Home,
        KeyModifiers::NONE,
    )));
    e.handle_input(&shift_event(KeyCode::Delete));
    assert_eq!(e.get_text(), "ello");
}
