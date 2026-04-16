//! Tests for the keybindings manager.
//!
//! These cover the "don't evict defaults" rule: user overrides on one
//! action must not silently drop another action's default keys, even
//! when the same key appears on both. Conflicts are only reported for
//! keys the user themselves bound to two different actions.
//!
//! The bottom of the file covers the process-scoped accessor
//! ([`aj_tui::keybindings::get`] / `set_user_bindings` / `set_manager` /
//! `reset`). Those tests must run serially because they mutate global
//! state, and they reset to defaults at the top so failed earlier
//! tests do not poison later ones.

use aj_tui::keybindings::{self, KeybindingConflict, KeybindingsManager, tui_keybindings};
use serial_test::serial;

#[test]
fn does_not_evict_selector_confirm_when_input_submit_is_rebound() {
    let kbm = KeybindingsManager::new(
        tui_keybindings(),
        [("tui.input.submit", vec!["enter", "ctrl+enter"])],
    );

    assert_eq!(kbm.get_keys("tui.input.submit"), &["enter", "ctrl+enter"]);
    // `tui.select.confirm` still defaults to `enter`.
    assert_eq!(kbm.get_keys("tui.select.confirm"), &["enter"]);
}

#[test]
fn does_not_evict_cursor_bindings_when_another_action_reuses_the_same_key() {
    let kbm = KeybindingsManager::new(tui_keybindings(), [("tui.select.up", vec!["up", "ctrl+p"])]);

    assert_eq!(kbm.get_keys("tui.select.up"), &["up", "ctrl+p"]);
    // `tui.editor.cursorUp` still defaults to `up`.
    assert_eq!(kbm.get_keys("tui.editor.cursorUp"), &["up"]);
}

#[test]
fn still_reports_direct_user_binding_conflicts_without_evicting_defaults() {
    let kbm = KeybindingsManager::new(
        tui_keybindings(),
        [
            ("tui.input.submit", "ctrl+x"),
            ("tui.select.confirm", "ctrl+x"),
        ],
    );

    assert_eq!(
        kbm.get_conflicts(),
        &[KeybindingConflict {
            key: "ctrl+x".to_string(),
            keybindings: vec![
                "tui.input.submit".to_string(),
                "tui.select.confirm".to_string(),
            ],
        }]
    );
    // Cursor-left defaults are untouched.
    assert_eq!(kbm.get_keys("tui.editor.cursorLeft"), &["left", "ctrl+b"]);
}

#[test]
fn resolved_bindings_list_every_action_in_definition_order() {
    let kbm = KeybindingsManager::new(tui_keybindings(), Vec::<(&str, &str)>::new());
    let resolved = kbm.get_resolved_bindings();

    // First few definitions in order.
    assert_eq!(resolved[0].0, "tui.editor.cursorUp");
    assert_eq!(resolved[0].1, vec!["up"]);
    assert_eq!(resolved[1].0, "tui.editor.cursorDown");
    assert_eq!(resolved[2].0, "tui.editor.cursorLeft");
    assert_eq!(resolved[2].1, vec!["left", "ctrl+b"]);

    // Totals match the definition list.
    assert_eq!(resolved.len(), tui_keybindings().len());
}

#[test]
fn set_user_bindings_replaces_previous_overrides_and_recomputes() {
    let mut kbm = KeybindingsManager::new(tui_keybindings(), [("tui.input.submit", "ctrl+x")]);
    assert_eq!(kbm.get_keys("tui.input.submit"), &["ctrl+x"]);

    kbm.set_user_bindings(Vec::<(&str, &str)>::new());
    assert_eq!(kbm.get_keys("tui.input.submit"), &["enter"]);
    assert!(kbm.get_conflicts().is_empty());
}

#[test]
fn matches_uses_the_resolved_key_list_for_an_action() {
    let kbm = KeybindingsManager::new(tui_keybindings(), Vec::<(&str, &str)>::new());

    use aj_tui::keys::Key;
    assert!(kbm.matches(&Key::enter(), "tui.input.submit"));
    assert!(!kbm.matches(&Key::tab(), "tui.input.submit"));
    // ctrl+c is bound to both tui.input.copy and tui.select.cancel by
    // default; both should match.
    assert!(kbm.matches(&Key::ctrl('c'), "tui.input.copy"));
    assert!(kbm.matches(&Key::ctrl('c'), "tui.select.cancel"));
}

// ---------------------------------------------------------------------------
// Process-scoped accessor (`get`, `set_user_bindings`, `set_manager`, `reset`)
// ---------------------------------------------------------------------------

#[test]
#[serial(global_keybindings)]
fn global_get_returns_default_keys_for_known_actions() {
    keybindings::reset();
    let kb = keybindings::get();
    assert_eq!(kb.get_keys("tui.input.submit"), &["enter"]);
    assert_eq!(kb.get_keys("tui.editor.cursorLeft"), &["left", "ctrl+b"]);
}

#[test]
#[serial(global_keybindings)]
fn global_set_user_bindings_takes_effect_for_subsequent_get() {
    keybindings::reset();
    keybindings::set_user_bindings([("tui.input.submit", vec!["enter", "ctrl+enter"])]);
    let kb = keybindings::get();
    assert_eq!(kb.get_keys("tui.input.submit"), &["enter", "ctrl+enter"]);
    // Other actions still default.
    assert_eq!(kb.get_keys("tui.select.confirm"), &["enter"]);
    drop(kb);
    keybindings::reset();
}

#[test]
#[serial(global_keybindings)]
fn global_reset_drops_user_overrides() {
    keybindings::reset();
    keybindings::set_user_bindings([("tui.input.submit", "ctrl+x")]);
    assert_eq!(keybindings::get().get_keys("tui.input.submit"), &["ctrl+x"]);

    keybindings::reset();
    assert_eq!(keybindings::get().get_keys("tui.input.submit"), &["enter"]);
}

#[test]
#[serial(global_keybindings)]
fn global_set_manager_swaps_definitions_wholesale() {
    keybindings::reset();
    let custom = KeybindingsManager::new(
        vec![(
            "tui.custom.action".to_string(),
            aj_tui::keybindings::KeybindingDefinition::new("ctrl+x", "Custom"),
        )],
        Vec::<(&str, &str)>::new(),
    );
    keybindings::set_manager(custom);

    let kb = keybindings::get();
    assert_eq!(kb.get_keys("tui.custom.action"), &["ctrl+x"]);
    // Defaults from `tui_keybindings()` are gone.
    assert!(kb.get_keys("tui.input.submit").is_empty());
    drop(kb);
    keybindings::reset();
}

#[test]
#[serial(global_keybindings)]
fn global_matches_returns_true_for_a_user_bound_alternate() {
    keybindings::reset();
    keybindings::set_user_bindings([("tui.input.submit", vec!["enter", "ctrl+enter"])]);

    use aj_tui::keys::Key;
    let kb = keybindings::get();
    // Both the original `enter` binding and the new `ctrl+enter` alternate
    // are recognized for `tui.input.submit`.
    assert!(kb.matches(&Key::enter(), "tui.input.submit"));
    let ctrl_enter = aj_tui::keys::InputEvent::Key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::CONTROL,
    ));
    assert!(kb.matches(&ctrl_enter, "tui.input.submit"));
    drop(kb);
    keybindings::reset();
}
