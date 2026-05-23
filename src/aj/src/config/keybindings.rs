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

/// Action ID for the "fold / unfold thinking blocks" toggle.
pub const ACTION_THINKING_TOGGLE: &str = "aj.thinking.toggle";

/// Action ID for the "expand / collapse tool output" global toggle.
///
/// Bound by default to `alt+o`. Flipping it walks every
/// `ToolExecutionComponent` in the chat scrollback and switches
/// between the compact (head- or tail-truncated body) and the full
/// rendering. Tool outputs default to compact; the keybinding is
/// the only way to reveal the full body, so the action ID is also
/// surfaced in the on-screen hint line so users can discover it
/// without consulting docs.
pub const ACTION_TOOLS_EXPAND: &str = "aj.tools.expand";

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
    fn aj_tools_expand_defaults_to_alt_o() {
        let kbm = KeybindingsManager::new(all_keybindings(), Vec::<(String, Vec<KeyId>)>::new());
        assert_eq!(kbm.get_keys(ACTION_TOOLS_EXPAND), &["alt+o".to_string()]);
    }

    #[test]
    fn install_global_manager_makes_action_visible_via_global_get() {
        install_global_manager_defaults();
        let kb = keybindings::get();
        assert_eq!(kb.get_keys(ACTION_THINKING_TOGGLE), &["alt+t".to_string()]);
    }
}
