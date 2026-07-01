//! Frontend-agnostic keybinding data: the `aj.*` action-ID constants
//! and the fixed-chord display labels.
//!
//! These are inert string constants shared by the command catalog
//! ([`crate::commands`]) and by each frontend's key-matching layer. The
//! manager machinery that resolves chords to actions (and merges these
//! with a backend's own bindings) lives per-binary, since it is bound to
//! that backend's key types.

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

/// Action ID for the "clear the selected project override" chord in
/// the project settings window.
///
/// Bound by default to `ctrl+x`. The project settings window intercepts
/// it on the main list: when the highlighted row is set by the project
/// layer, the override is removed and the row reverts to the inherited
/// user value. Handled inside that window only, so it never interferes
/// with the row search box (which a plain key would feed). Inert on
/// already-inherited rows and in the user settings window.
pub const ACTION_SETTINGS_CLEAR: &str = "aj.settings.clear";

/// The `aj.*` actions with their default chord and description, in the
/// order help screens and the keybindings manager list them.
///
/// Each row is `(action_id, default_chord, description)`: the action ID
/// (one of the `ACTION_*` consts above), the chord it binds to by
/// default, and the human-readable label. Frontends turn this into their
/// own binding-definition type against their key-matching layer, so the
/// data stays here while the manager machinery lives per-binding.
pub const AJ_KEYBINDINGS: &[(&str, &str, &str)] = &[
    (
        ACTION_THINKING_TOGGLE,
        "alt+t",
        "Toggle visibility of assistant thinking blocks",
    ),
    (ACTION_TOOLS_EXPAND, "alt+o", "Toggle expanded tool output"),
    (
        ACTION_CLIPBOARD_PASTE_IMAGE,
        "ctrl+v",
        "Paste image from clipboard",
    ),
    (ACTION_PALETTE_OPEN, "ctrl+o", "Open command palette"),
    (
        ACTION_OVERLAY_CLOSE_ALL,
        "ctrl+c",
        "Close all open overlays",
    ),
    (
        ACTION_HISTORY_TOGGLE_SCOPE,
        "ctrl+t",
        "Toggle prompt-history scope (workspace / all)",
    ),
    (ACTION_HISTORY_OPEN, "ctrl+r", "Open prompt-history search"),
    (ACTION_AGENT_PICKER, "alt+a", "Open agent picker"),
    (
        ACTION_AGENT_TOGGLE_SCOPE,
        "ctrl+t",
        "Toggle agent-picker scope (running / all)",
    ),
    (
        ACTION_TASK_KILL,
        "ctrl+k",
        "Kill the selected background task",
    ),
    (
        ACTION_SUBMIT_STEERING,
        "alt+enter",
        "Queue / send the message as steering",
    ),
    (
        ACTION_DEQUEUE,
        "alt+up",
        "Pull the queued message back into the editor",
    ),
    (
        ACTION_SETTINGS_CLEAR,
        "ctrl+x",
        "Clear the selected project override",
    ),
];

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
