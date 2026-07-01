//! `aj`-level keybindings layered on top of [`aj_tui::keybindings`].
//!
//! The TUI crate ships generic editor / input / selection bindings via
//! [`aj_tui::keybindings::tui_keybindings`]. This module adds the
//! `aj.*` actions the interactive binary needs (the thinking-block
//! toggle, the tool-output expansion toggle) and installs the
//! combined registry into the process-wide manager.
//!
//! Callers look up bindings through `aj_tui::keybindings::get()` and
//! match keys against the action ID (`"aj.thinking.toggle"` etc.), so
//! the resolved keys are surfaced both for input dispatch and for
//! user-facing hints (e.g. the collapsed thinking-block placeholder).

use aj_tui::keybindings::{
    KeyId, KeybindingDefinition, KeybindingDefinitions, KeybindingsManager, set_manager,
    tui_keybindings,
};

// The `aj.*` action-ID constants and the `fixed_keys` display labels
// are frontend-agnostic data and live in `aj_app::keybindings`. They are
// re-exported here so this crate's manager machinery and the `aj.*`
// binding table below keep referring to them by bare name, and so
// `crate::config::keybindings::{ACTION_*, fixed_keys}` paths elsewhere
// in `aj` keep resolving.
pub use aj_app::keybindings::*;

/// Built-in `aj`-level keybinding definitions.
///
/// Returned as a fresh `Vec` so callers can extend or filter before
/// handing it to a [`KeybindingsManager`].
pub fn aj_keybindings() -> KeybindingDefinitions {
    use KeybindingDefinition as K;
    vec![
        (
            ACTION_THINKING_TOGGLE.to_string(),
            K::new("alt+t", "Toggle visibility of assistant thinking blocks"),
        ),
        (
            ACTION_TOOLS_EXPAND.to_string(),
            K::new("alt+o", "Toggle expanded tool output"),
        ),
        (
            ACTION_CLIPBOARD_PASTE_IMAGE.to_string(),
            K::new("ctrl+v", "Paste image from clipboard"),
        ),
        (
            ACTION_PALETTE_OPEN.to_string(),
            K::new("ctrl+o", "Open command palette"),
        ),
        (
            ACTION_OVERLAY_CLOSE_ALL.to_string(),
            K::new("ctrl+c", "Close all open overlays"),
        ),
        (
            ACTION_HISTORY_TOGGLE_SCOPE.to_string(),
            K::new("ctrl+t", "Toggle prompt-history scope (workspace / all)"),
        ),
        (
            ACTION_HISTORY_OPEN.to_string(),
            K::new("ctrl+r", "Open prompt-history search"),
        ),
        (
            ACTION_AGENT_PICKER.to_string(),
            K::new("alt+a", "Open agent picker"),
        ),
        (
            ACTION_AGENT_TOGGLE_SCOPE.to_string(),
            K::new("ctrl+t", "Toggle agent-picker scope (running / all)"),
        ),
        (
            ACTION_TASK_KILL.to_string(),
            K::new("ctrl+k", "Kill the selected background task"),
        ),
        (
            ACTION_SUBMIT_STEERING.to_string(),
            K::new("alt+enter", "Queue / send the message as steering"),
        ),
        (
            ACTION_DEQUEUE.to_string(),
            K::new("alt+up", "Pull the queued message back into the editor"),
        ),
        (
            ACTION_SETTINGS_CLEAR.to_string(),
            K::new("ctrl+x", "Clear the selected project override"),
        ),
    ]
}

/// Combined definitions: every `tui.*` action followed by every
/// `aj.*` action. Order matters for [`KeybindingsManager::get_resolved_bindings`]
/// (deterministic listings); the `tui.*` block stays first so help
/// screens keep their existing ordering.
pub fn all_keybindings() -> KeybindingDefinitions {
    let mut defs = tui_keybindings();
    defs.extend(aj_keybindings());
    defs
}

/// Install the combined `tui.*` + `aj.*` registry into the process-
/// wide [`KeybindingsManager`]. Pass user overrides (parsed from
/// `config.toml`) as `user_bindings`; pass an empty iterator if no
/// overrides apply.
///
/// Safe to call multiple times — the last call wins, matching
/// [`set_manager`]'s semantics. Should be invoked once at startup
/// before any component looks up a key.
pub fn install_global_manager<U, S, K>(user_bindings: U)
where
    U: IntoIterator<Item = (S, K)>,
    S: Into<String>,
    K: aj_tui::keybindings::IntoKeyList,
{
    set_manager(KeybindingsManager::new(all_keybindings(), user_bindings));
}

/// No-override convenience wrapper around [`install_global_manager`].
pub fn install_global_manager_defaults() {
    install_global_manager(Vec::<(String, Vec<KeyId>)>::new());
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_tui::keybindings;

    #[test]
    fn aj_thinking_toggle_defaults_to_alt_t() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(kbm.get_keys(ACTION_THINKING_TOGGLE), &["alt+t".to_string()]);
    }

    #[test]
    fn aj_clipboard_paste_image_defaults_to_ctrl_v() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(
            kbm.get_keys(ACTION_CLIPBOARD_PASTE_IMAGE),
            &["ctrl+v".to_string()]
        );
    }

    #[test]
    fn aj_tools_expand_defaults_to_alt_o() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(kbm.get_keys(ACTION_TOOLS_EXPAND), &["alt+o".to_string()]);
    }

    #[test]
    fn aj_palette_open_defaults_to_ctrl_o() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(kbm.get_keys(ACTION_PALETTE_OPEN), &["ctrl+o".to_string()]);
    }

    #[test]
    fn aj_overlay_close_all_defaults_to_ctrl_c() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(
            kbm.get_keys(ACTION_OVERLAY_CLOSE_ALL),
            &["ctrl+c".to_string()]
        );
    }

    #[test]
    fn aj_history_open_defaults_to_ctrl_r() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(kbm.get_keys(ACTION_HISTORY_OPEN), &["ctrl+r".to_string()]);
    }

    #[test]
    fn aj_agent_picker_defaults_to_alt_a() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(kbm.get_keys(ACTION_AGENT_PICKER), &["alt+a".to_string()]);
    }

    #[test]
    fn aj_agent_toggle_scope_defaults_to_ctrl_t() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(
            kbm.get_keys(ACTION_AGENT_TOGGLE_SCOPE),
            &["ctrl+t".to_string()]
        );
    }

    #[test]
    fn aj_task_kill_defaults_to_ctrl_k() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(kbm.get_keys(ACTION_TASK_KILL), &["ctrl+k".to_string()]);
    }

    #[test]
    fn aj_submit_steering_defaults_to_alt_enter() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(
            kbm.get_keys(ACTION_SUBMIT_STEERING),
            &["alt+enter".to_string()]
        );
    }

    #[test]
    fn aj_dequeue_defaults_to_alt_up() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(kbm.get_keys(ACTION_DEQUEUE), &["alt+up".to_string()]);
    }

    #[test]
    fn install_global_manager_makes_action_visible_via_global_get() {
        install_global_manager_defaults();
        let kb = keybindings::get();
        assert_eq!(kb.get_keys(ACTION_THINKING_TOGGLE), &["alt+t".to_string()]);
    }
}
