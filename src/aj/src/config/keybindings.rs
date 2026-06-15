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

/// Action ID for the "paste image from system clipboard" chord.
///
/// Bound by default to `ctrl+v`. The interactive loop intercepts the
/// keystroke before the editor sees it, reads the clipboard, writes
/// any image payload to a tempfile, and inserts that file's path
/// into the editor as plain text. On submit, the model reads the
/// file through the `read_file` tool. A clipboard miss is a silent
/// no-op — users expect Ctrl+V to be benign.
pub const ACTION_CLIPBOARD_PASTE_IMAGE: &str = "aj.clipboard.paste_image";

/// Action ID for the "open command palette" chord.
///
/// Bound by default to `ctrl+o`. The interactive loop intercepts the
/// keystroke globally (before any component sees it) and opens the
/// command palette overlay. When a capturing overlay is already up
/// the listener bails out, so the chord doesn't interrupt an open
/// selector. The same overlay can also be opened by typing `/` at
/// an empty prompt or by submitting `/palette`.
pub const ACTION_PALETTE_OPEN: &str = "aj.palette.open";

/// Closes every open overlay in one keystroke regardless of
/// nesting depth — used as a "bail out completely" shortcut.
/// Default binding: `ctrl+c`. The interactive loop intercepts the
/// keystroke before `tui.handle_input` when any overlay is open
/// and consumes the event so the selector doesn't also run its
/// cancel path.
pub const ACTION_OVERLAY_CLOSE_ALL: &str = "aj.overlay.close_all";

/// Toggles the prompt-history search between the current workspace
/// and all workspaces. Default binding: `ctrl+t`. Handled inside
/// the prompt-history overlay; the default scope is the current
/// workspace.
pub const ACTION_HISTORY_TOGGLE_SCOPE: &str = "aj.history.toggle_scope";

/// Action ID for the "open prompt-history search" chord.
///
/// Bound by default to `ctrl+r`. The interactive loop intercepts the
/// keystroke globally (before any component sees it) and opens the
/// prompt-history search overlay, exactly as if the user had typed
/// `/history`. Because it is dispatched directly (not via the
/// command palette), the overlay has no parent palette: `Esc`
/// closes it back to the editor rather than popping to the palette.
/// Inert while a capturing overlay is already up.
pub const ACTION_HISTORY_OPEN: &str = "aj.history.open";

/// Action ID for the "open agent picker" chord.
///
/// Bound by default to `alt+a`. The interactive loop intercepts the
/// keystroke globally and opens the agent-picker overlay, which
/// switches the chat view between the main agent and any sub-agent.
/// Inert while a capturing overlay is already up.
pub const ACTION_AGENT_PICKER: &str = "aj.agent.open";

/// Toggles the agent picker between showing only running sub-agents
/// and all sub-agents in the session. Default binding: `ctrl+t`.
/// Handled inside the agent-picker overlay (contextual; only the
/// focused picker reads it), mirroring the prompt-history scope
/// toggle's key and feel.
pub const ACTION_AGENT_TOGGLE_SCOPE: &str = "aj.agent.toggle_scope";

/// Kills the background task selected in the agent picker. Default
/// binding: `ctrl+k`. Handled inside the agent-picker overlay
/// (contextual; only the focused picker reads it); the host routes
/// the resulting outcome to the task registry's kill.
pub const ACTION_TASK_KILL: &str = "aj.task.kill";

/// Action ID for the "submit as a steering message" chord.
///
/// Bound by default to `alt+enter`. The interactive loop intercepts
/// the keystroke before the editor sees it (so it never inserts a
/// newline). While the viewed agent is busy it queues the editor text
/// as a steering message (injected right after the next tool call),
/// escalating any pending follow-up; while idle it starts a normal
/// turn. Repurposing `alt+enter` drops its editor newline-fallback
/// role — `shift+enter` and `\`+Enter remain for newline.
pub const ACTION_SUBMIT_STEERING: &str = "aj.message.steer";

/// Action ID for the "pull a queued message back into the editor"
/// chord.
///
/// Bound by default to `alt+up`. The interactive loop intercepts it
/// before the editor and, when a message is queued for the viewed
/// agent, removes it from the queue and prepends it to the editor.
/// `up` / `ctrl+p` also yank, but only when the editor is empty (so
/// they keep their normal history-navigation role otherwise); this
/// chord yanks regardless of editor contents.
pub const ACTION_DEQUEUE: &str = "aj.message.dequeue";

/// Canonical display labels for keyboard chords that are deliberately
/// fixed terminal conventions rather than rebindable actions.
///
/// The behavior behind these chords is hardcoded — `Ctrl+C` is matched
/// as `is_ctrl('c')` in the interactive input loop (cancel the running
/// turn, or quit when idle) and `Ctrl+Y` as `is_ctrl('y')` in the login
/// dialog (copy the authorization URL). Because they are not registered
/// with the keybindings manager, on-screen messages can't resolve them
/// through `format_action_shortcut`. Keeping one spelling here gives
/// those messages a single source of truth so they can't drift from the
/// canonical `Ctrl+C` display form.
pub mod fixed_keys {
    /// Cancel the running turn, or quit when idle (SIGINT-style).
    pub const CTRL_C: &str = "Ctrl+C";

    /// Copy the authorization URL to the clipboard (login dialog).
    pub const CTRL_Y: &str = "Ctrl+Y";
}

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
