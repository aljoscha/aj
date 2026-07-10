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

/// Action ID for the "scroll the transcript up a page" chord.
///
/// Bound by default to `pageup`. The chord is intercepted before the
/// editor sees it (the editor's own PageUp scroll is superseded by
/// chat page-scroll), so the transcript scrolls up a page even while
/// the editor is focused. Inert while a capturing overlay is up, which
/// then owns its own PageUp.
pub const ACTION_CHAT_PAGE_UP: &str = "aj.chat.page_up";

/// Action ID for the "scroll the transcript down a page" chord.
///
/// Bound by default to `pagedown`. The counterpart of
/// [`ACTION_CHAT_PAGE_UP`]: intercepted before the editor and inert
/// while a capturing overlay is up. Scrolling back to the bottom
/// re-engages follow-tail.
pub const ACTION_CHAT_PAGE_DOWN: &str = "aj.chat.page_down";

/// Action ID for the "scroll the transcript to the top" chord.
///
/// Bound by default to `home`. Intercepted in the capture phase before
/// the editor sees it, so it scrolls the transcript even while composing.
/// The editor's own line-start motion stays on Ctrl+A, which this chord
/// does not touch. Mode-aware: in editor mode it pins the viewport to the
/// absolute top and disengages follow-tail, in transcript-focus mode it
/// moves the item cursor to the first item. Inert while a capturing
/// overlay is up, which then owns its own Home.
pub const ACTION_CHAT_SCROLL_TOP: &str = "aj.chat.scroll_top";

/// Action ID for the "scroll the transcript to the bottom" chord.
///
/// Bound by default to `end`. The counterpart of
/// [`ACTION_CHAT_SCROLL_TOP`]: intercepted before the editor (the
/// editor's line-end motion stays on Ctrl+E) and inert while a capturing
/// overlay is up. In editor mode it re-engages follow-tail so the
/// viewport lands at the end and tracks streamed content, in
/// transcript-focus mode it moves the item cursor to the last item.
pub const ACTION_CHAT_SCROLL_BOTTOM: &str = "aj.chat.scroll_bottom";

/// Action ID for the "focus the transcript for keyboard navigation" chord.
///
/// Bound by default to `tab`. It moves keyboard focus from the editor onto the
/// chat transcript and steps through past user messages (Spec E section 1,
/// transcript-focus mode). It matches in the capture phase but is gated to the
/// autocomplete popup being closed, so Tab focuses the transcript even with a
/// draft in the editor, and only an open popup keeps Tab for applying the
/// highlighted completion. Inert while a capturing overlay is up. Esc returns
/// focus to the editor.
pub const ACTION_TRANSCRIPT_FOCUS: &str = "aj.transcript.focus";

/// Action ID for the "copy the focused message" chord.
///
/// Bound by default to `y`. Live only in transcript-focus mode (Spec E
/// section 2): the frontend gates it on the transcript being focused, so with
/// the editor focused `y` types normally. Pressing it copies the whole focused
/// user message to the system clipboard through the same OSC 52 path the mouse
/// select-to-copy uses.
pub const ACTION_COPY_MESSAGE: &str = "aj.transcript.copy_message";

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

/// Action ID for the "spend a rate-limit reset credit" chord in the usage
/// overlay.
///
/// Bound by default to `r`. The usage overlay intercepts it on its
/// read-only page to start the reset flow, when a provider reports
/// available resets and has a matching source. Handled inside that
/// overlay only (contextual). Like [`ACTION_HISTORY_TOGGLE_SCOPE`] it is a
/// table-only row: it is not compiled into the global keymap, so it exists
/// purely to resolve the footer-hint label through the shared binding
/// data. The in-overlay handling is a fixed convention for now.
pub const ACTION_USAGE_RESET: &str = "aj.usage.reset";

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
        ACTION_CHAT_PAGE_UP,
        "pageup",
        "Scroll the transcript up a page",
    ),
    (
        ACTION_CHAT_PAGE_DOWN,
        "pagedown",
        "Scroll the transcript down a page",
    ),
    (
        ACTION_CHAT_SCROLL_TOP,
        "home",
        "Scroll the transcript to the top",
    ),
    (
        ACTION_CHAT_SCROLL_BOTTOM,
        "end",
        "Scroll the transcript to the bottom",
    ),
    (
        ACTION_TRANSCRIPT_FOCUS,
        "tab",
        "Focus the transcript to step through past messages",
    ),
    (ACTION_COPY_MESSAGE, "y", "Copy the focused message"),
    (
        ACTION_SETTINGS_CLEAR,
        "ctrl+x",
        "Clear the selected project override",
    ),
    (
        ACTION_USAGE_RESET,
        "r",
        "Spend a rate-limit reset credit (usage overlay)",
    ),
];

/// The default chord for `action_id`, from [`AJ_KEYBINDINGS`]. `None`
/// for unknown action IDs.
pub fn default_chord(action_id: &str) -> Option<&'static str> {
    AJ_KEYBINDINGS
        .iter()
        .find(|(id, _, _)| *id == action_id)
        .map(|(_, chord, _)| *chord)
}

/// Convert a canonical keybinding string like `"ctrl+o"` or
/// `"alt+shift+t"` or `"escape"` into the display form
/// `"Ctrl+O"` / `"Alt+Shift+T"` / `"Esc"` used in UI surfaces
/// (palette shortcut column, overlay subtitles, hint lines).
///
/// Splits on `+`, maps modifier and named-key segments to their
/// display labels, title-cases everything else, and rejoins.
///
/// NOTE: `aj-tui` carries the same formatter for the `aj` binary
/// (bound to its own keybindings manager). Both format the canonical
/// chord grammar of [`AJ_KEYBINDINGS`], so their display spellings
/// must agree.
pub fn format_keybinding(canonical: &str) -> String {
    canonical
        .split('+')
        .map(format_key_segment)
        .collect::<Vec<_>>()
        .join("+")
}

fn format_key_segment(seg: &str) -> String {
    let lower = seg.to_ascii_lowercase();
    match lower.as_str() {
        "ctrl" => "Ctrl".to_string(),
        "alt" => "Alt".to_string(),
        "shift" => "Shift".to_string(),
        // `super` is the only "windows/command/meta" modifier the
        // canonical grammar recognizes (`crate::actions::parse_chord`
        // deliberately rejects `meta`/`hyper`). We display it under the
        // same `super` spelling so the label can't advertise a modifier
        // the matcher rejects. Unknown spellings like `cmd`/`meta` fall
        // through to the title-case arm, the same as any other
        // unrecognized segment.
        "super" => "Super".to_string(),
        "escape" | "esc" => "Esc".to_string(),
        "enter" | "return" => "Enter".to_string(),
        "tab" => "Tab".to_string(),
        "space" => "Space".to_string(),
        "backspace" => "Backspace".to_string(),
        "delete" | "del" => "Del".to_string(),
        "home" => "Home".to_string(),
        "end" => "End".to_string(),
        "pageup" => "PgUp".to_string(),
        "pagedown" => "PgDn".to_string(),
        "left" => "Left".to_string(),
        "right" => "Right".to_string(),
        "up" => "Up".to_string(),
        "down" => "Down".to_string(),
        "insert" => "Insert".to_string(),
        _ => {
            // Title-case: uppercase the first character, leave the
            // rest as-is so symbol-only segments like `]` survive
            // and function keys like `f1` become `F1`.
            let mut chars = seg.chars();
            match chars.next() {
                Some(c) => c.to_ascii_uppercase().to_string() + chars.as_str(),
                None => String::new(),
            }
        }
    }
}

/// The default chord for `action_id` formatted for display via
/// [`format_keybinding`], for hint labels. `None` for unknown action
/// IDs.
///
/// This resolves the built-in default only. Once user `[keybindings]`
/// overrides land (see `crate::actions::default_global_bindings`), hint
/// surfaces resolve through the merged bindings instead.
pub fn default_action_shortcut(action_id: &str) -> Option<String> {
    default_chord(action_id).map(format_keybinding)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_keybinding_handles_modifiers_and_named_keys() {
        assert_eq!(format_keybinding("ctrl+o"), "Ctrl+O");
        assert_eq!(format_keybinding("escape"), "Esc");
        assert_eq!(format_keybinding("alt+shift+t"), "Alt+Shift+T");
        assert_eq!(format_keybinding("ctrl+left"), "Ctrl+Left");
        assert_eq!(format_keybinding("enter"), "Enter");
        assert_eq!(format_keybinding("pageUp"), "PgUp");
        assert_eq!(format_keybinding("ctrl+]"), "Ctrl+]");
        assert_eq!(format_keybinding("super+k"), "Super+K");
    }

    #[test]
    fn format_keybinding_does_not_advertise_unmatched_modifiers() {
        // `cmd`/`meta` are not part of the canonical grammar
        // (`crate::actions::parse_chord` rejects them, so the binding
        // never fires). The display side must not pretty-print them as
        // a recognized modifier: they title-case like any other unknown
        // segment, so a hint can't pretend the binding is valid.
        assert_eq!(format_keybinding("cmd+k"), "Cmd+K");
        assert_eq!(format_keybinding("meta+k"), "Meta+K");
    }

    #[test]
    fn default_action_shortcut_resolves_the_table() {
        assert_eq!(
            default_action_shortcut(ACTION_TOOLS_EXPAND).as_deref(),
            Some("Alt+O")
        );
        assert_eq!(
            default_action_shortcut(ACTION_SUBMIT_STEERING).as_deref(),
            Some("Alt+Enter")
        );
        assert_eq!(
            default_action_shortcut(ACTION_CHAT_PAGE_UP).as_deref(),
            Some("PgUp")
        );
        assert_eq!(
            default_action_shortcut(ACTION_CHAT_PAGE_DOWN).as_deref(),
            Some("PgDn")
        );
        assert_eq!(default_action_shortcut("aj.unknown"), None);
    }
}
