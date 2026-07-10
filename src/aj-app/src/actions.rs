//! The typed keymap data layer: the global action vocabulary, the chord
//! grammar parser, and the default binding table.
//!
//! This module is the frontend-neutral half of the keymap boundary. It
//! knows the `aj.*` action IDs and their default chords (from
//! [`crate::keybindings::AJ_KEYBINDINGS`]) and compiles them into
//! [`ChordSpec`]s, a representation with no key-event types in it. A
//! frontend translates a `ChordSpec` into its own activator type (the
//! vaxis frontend maps [`ChordKey::Named`] names through vaxis's key-name
//! table) and attaches its own context predicates and handlers.
//!
//! Only globally-dispatched actions appear here. The overlay-local
//! bindings in `AJ_KEYBINDINGS` (`aj.history.toggle_scope`,
//! `aj.agent.toggle_scope`, `aj.task.kill`, `aj.settings.clear`) are
//! handled at-target by the focused overlay widget, so they are that
//! widget's data, not part of the global action vocabulary.

use crate::keybindings::{
    ACTION_AGENT_PICKER, ACTION_CHAT_PAGE_DOWN, ACTION_CHAT_PAGE_UP, ACTION_CHAT_SCROLL_BOTTOM,
    ACTION_CHAT_SCROLL_TOP, ACTION_CLIPBOARD_PASTE_IMAGE, ACTION_COPY_MESSAGE, ACTION_DEQUEUE,
    ACTION_HISTORY_OPEN, ACTION_OVERLAY_CLOSE_ALL, ACTION_PALETTE_OPEN, ACTION_SUBMIT_STEERING,
    ACTION_THINKING_TOGGLE, ACTION_TOOLS_EXPAND, ACTION_TRANSCRIPT_FOCUS, default_chord,
};

/// A global keymap action, the typed counterpart of the `aj.*` action-ID
/// strings in [`crate::keybindings`].
///
/// `CancelTurn` and `Quit` have no `AJ_KEYBINDINGS` row: they are the two
/// rungs of the fixed Ctrl+C ladder (see
/// [`crate::keybindings::fixed_keys`]), a deliberate terminal convention
/// rather than a rebindable action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AjAction {
    /// Toggle visibility of assistant thinking blocks
    /// (`aj.thinking.toggle`).
    ThinkingToggle,
    /// Toggle expanded tool output (`aj.tools.expand`).
    ToolsExpand,
    /// Paste an image from the system clipboard
    /// (`aj.clipboard.paste_image`).
    PasteImage,
    /// Open the command palette (`aj.palette.open`).
    PaletteOpen,
    /// Close every open overlay (`aj.overlay.close_all`).
    CloseAllOverlays,
    /// Open the prompt-history search (`aj.history.open`).
    HistoryOpen,
    /// Open the agent picker (`aj.agent.open`).
    AgentPickerOpen,
    /// Submit / queue the editor text as a steering message
    /// (`aj.message.steer`).
    Steer,
    /// Pull the queued message back into the editor
    /// (`aj.message.dequeue`).
    Dequeue,
    /// Scroll the chat transcript up one viewport page
    /// (`aj.chat.page_up`).
    ChatPageUp,
    /// Scroll the chat transcript down one viewport page
    /// (`aj.chat.page_down`).
    ChatPageDown,
    /// Scroll the chat transcript to the top (`aj.chat.scroll_top`).
    ChatScrollToTop,
    /// Scroll the chat transcript to the bottom (`aj.chat.scroll_bottom`).
    ChatScrollToBottom,
    /// Focus the chat transcript for keyboard navigation
    /// (`aj.transcript.focus`).
    TranscriptFocus,
    /// Copy the focused user message to the clipboard, live only in
    /// transcript-focus mode (`aj.transcript.copy_message`).
    CopyMessage,
    /// Cancel the viewed agent's running turn (the Ctrl+C ladder's first
    /// rung).
    CancelTurn,
    /// Quit the application (the Ctrl+C Ctrl+C ladder's second rung).
    Quit,
}

/// The key half of a parsed chord, with no key-event types in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordKey {
    /// A printable character, folded to lowercase by the parser.
    Char(char),
    /// A named non-printable key. The names are the canonical snake_case
    /// spellings a frontend's key-name table resolves (`"enter"`,
    /// `"escape"`, `"page_up"`, ...), emitted from the closed set in
    /// [`parse_chord`].
    Named(&'static str),
    /// A function key, `F1` through `F35`.
    F(u8),
}

/// A parsed chord: one key plus the exact modifier set.
///
/// Modifiers match exactly. `shift` on a [`ChordKey::Char`] means the
/// terminal reports the shift bit alongside the character, which modern
/// (kitty-protocol) terminals do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordSpec {
    pub key: ChordKey,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub super_mod: bool,
}

/// Parse a chord descriptor like `"ctrl+shift+p"`, `"alt+enter"`, or
/// `"ctrl++"` (a literal `+` key).
///
/// Strict-rejection contract for unknown modifiers: a `+`-separated token
/// that is not `ctrl`, `alt`, `shift`, or `super` rejects the whole
/// descriptor (`meta` and `hyper` included). Silently dropping unknown
/// tokens would let a config typo change which keystrokes a binding fires
/// on, so we fail closed instead.
pub fn parse_chord(input: &str) -> Option<ChordSpec> {
    if input.is_empty() {
        return None;
    }
    let mut ctrl = false;
    let mut alt = false;
    let mut shift = false;
    let mut super_mod = false;
    let mut apply_modifier = |m: &str| -> bool {
        match m {
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "shift" => shift = true,
            "super" => super_mod = true,
            _ => return false,
        }
        true
    };

    let lower = input.to_ascii_lowercase();
    let mut parts: Vec<&str> = lower.split('+').collect();
    if parts.iter().any(|s| s.is_empty()) {
        // An empty segment can only come from a literal `+` key
        // (`"ctrl++"` splits into `["ctrl", "", ""]`): the key is `+`,
        // every non-empty segment is a modifier.
        parts.retain(|s| !s.is_empty());
        for modifier in &parts {
            if !apply_modifier(modifier) {
                return None;
            }
        }
        return Some(ChordSpec {
            key: ChordKey::Char('+'),
            ctrl,
            alt,
            shift,
            super_mod,
        });
    }

    let key = parts.pop()?;
    for modifier in &parts {
        if !apply_modifier(modifier) {
            return None;
        }
    }

    let key = match key {
        "enter" | "return" => ChordKey::Named("enter"),
        "escape" | "esc" => ChordKey::Named("escape"),
        "tab" => ChordKey::Named("tab"),
        "backspace" => ChordKey::Named("backspace"),
        "delete" => ChordKey::Named("delete"),
        "insert" => ChordKey::Named("insert"),
        "up" => ChordKey::Named("up"),
        "down" => ChordKey::Named("down"),
        "left" => ChordKey::Named("left"),
        "right" => ChordKey::Named("right"),
        "home" => ChordKey::Named("home"),
        "end" => ChordKey::Named("end"),
        "pageup" => ChordKey::Named("page_up"),
        "pagedown" => ChordKey::Named("page_down"),
        // Space is spelled as a name in the grammar but is an ordinary
        // printable character to the matcher.
        "space" => ChordKey::Char(' '),
        k if k.starts_with('f') && k.len() > 1 && k[1..].chars().all(|c| c.is_ascii_digit()) => {
            let n: u8 = k[1..].parse().ok()?;
            // F1..F35 is the range terminals report (and frontends name).
            if !(1..=35).contains(&n) {
                return None;
            }
            ChordKey::F(n)
        }
        k => {
            let mut chars = k.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            ChordKey::Char(ch)
        }
    };

    Some(ChordSpec {
        key,
        ctrl,
        alt,
        shift,
        super_mod,
    })
}

/// The dispatch phase a global binding wants, mirrored by the frontend
/// onto its keymap engine.
///
/// `Capture` chords pre-empt the focused widget (the editor never sees
/// them), `Bubble` chords are shadowable by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordPhase {
    Capture,
    Bubble,
}

/// One compiled default binding: the action, its parsed default chord,
/// and the dispatch phase it wants.
#[derive(Debug, Clone, Copy)]
pub struct GlobalBinding {
    pub action: AjAction,
    pub chord: ChordSpec,
    pub phase: ChordPhase,
}

/// The default global bindings, compiled from the `AJ_KEYBINDINGS` chord
/// strings so the two can't drift apart.
///
/// Every entry is capture-phase: these are the chords `aj` intercepts before
/// its editor sees the keystroke. [`AjAction::TranscriptFocus`] (bound to
/// `tab`) is capture-phase too, but its frontend predicate gates it to the
/// autocomplete popup being closed, so Tab focuses the transcript even with a
/// draft, and only an open popup keeps Tab for the editor to apply a completion
/// (Spec E section 1). The Ctrl+C ladder (`CancelTurn`, `Quit`) is not in the
/// table, see [`AjAction`].
///
/// TODO(aljoscha): merge the user's `[keybindings]` config over these
/// defaults (replace-not-extend per action, like `aj`'s manager).
/// Defaults only for now.
pub fn default_global_bindings() -> Vec<GlobalBinding> {
    let compiled = |action: AjAction, action_id: &str, phase: ChordPhase| {
        let chord = default_chord(action_id).expect("every global action has a default chord");
        GlobalBinding {
            action,
            chord: parse_chord(chord).expect("the default chords parse"),
            phase,
        }
    };
    use ChordPhase::Capture;
    vec![
        compiled(AjAction::ThinkingToggle, ACTION_THINKING_TOGGLE, Capture),
        compiled(AjAction::ToolsExpand, ACTION_TOOLS_EXPAND, Capture),
        compiled(AjAction::PasteImage, ACTION_CLIPBOARD_PASTE_IMAGE, Capture),
        compiled(AjAction::PaletteOpen, ACTION_PALETTE_OPEN, Capture),
        compiled(
            AjAction::CloseAllOverlays,
            ACTION_OVERLAY_CLOSE_ALL,
            Capture,
        ),
        compiled(AjAction::HistoryOpen, ACTION_HISTORY_OPEN, Capture),
        compiled(AjAction::AgentPickerOpen, ACTION_AGENT_PICKER, Capture),
        compiled(AjAction::Steer, ACTION_SUBMIT_STEERING, Capture),
        compiled(AjAction::Dequeue, ACTION_DEQUEUE, Capture),
        compiled(AjAction::ChatPageUp, ACTION_CHAT_PAGE_UP, Capture),
        compiled(AjAction::ChatPageDown, ACTION_CHAT_PAGE_DOWN, Capture),
        compiled(AjAction::ChatScrollToTop, ACTION_CHAT_SCROLL_TOP, Capture),
        compiled(
            AjAction::ChatScrollToBottom,
            ACTION_CHAT_SCROLL_BOTTOM,
            Capture,
        ),
        compiled(AjAction::TranscriptFocus, ACTION_TRANSCRIPT_FOCUS, Capture),
        compiled(AjAction::CopyMessage, ACTION_COPY_MESSAGE, Capture),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::AJ_KEYBINDINGS;

    fn chord(key: ChordKey, ctrl: bool, alt: bool, shift: bool, super_mod: bool) -> ChordSpec {
        ChordSpec {
            key,
            ctrl,
            alt,
            shift,
            super_mod,
        }
    }

    /// Every default chord in the shared table parses. This is the guard
    /// that keeps `AJ_KEYBINDINGS` inside the grammar `parse_chord`
    /// accepts, including the overlay-local rows that don't compile into
    /// the global table.
    #[test]
    fn every_aj_keybindings_default_parses() {
        for (action_id, chord, _) in AJ_KEYBINDINGS {
            assert!(
                parse_chord(chord).is_some(),
                "default chord {chord:?} for {action_id} must parse"
            );
        }
    }

    #[test]
    fn parses_the_default_global_chords() {
        let bindings = default_global_bindings();
        let spec = |action: AjAction| {
            bindings
                .iter()
                .find(|b| b.action == action)
                .expect("action bound")
                .chord
        };
        assert_eq!(
            spec(AjAction::ThinkingToggle),
            chord(ChordKey::Char('t'), false, true, false, false)
        );
        assert_eq!(
            spec(AjAction::ToolsExpand),
            chord(ChordKey::Char('o'), false, true, false, false)
        );
        assert_eq!(
            spec(AjAction::PasteImage),
            chord(ChordKey::Char('v'), true, false, false, false)
        );
        assert_eq!(
            spec(AjAction::PaletteOpen),
            chord(ChordKey::Char('o'), true, false, false, false)
        );
        assert_eq!(
            spec(AjAction::CloseAllOverlays),
            chord(ChordKey::Char('c'), true, false, false, false)
        );
        assert_eq!(
            spec(AjAction::HistoryOpen),
            chord(ChordKey::Char('r'), true, false, false, false)
        );
        assert_eq!(
            spec(AjAction::AgentPickerOpen),
            chord(ChordKey::Char('a'), false, true, false, false)
        );
        assert_eq!(
            spec(AjAction::Steer),
            chord(ChordKey::Named("enter"), false, true, false, false)
        );
        assert_eq!(
            spec(AjAction::Dequeue),
            chord(ChordKey::Named("up"), false, true, false, false)
        );
        assert_eq!(
            spec(AjAction::ChatPageUp),
            chord(ChordKey::Named("page_up"), false, false, false, false)
        );
        assert_eq!(
            spec(AjAction::ChatPageDown),
            chord(ChordKey::Named("page_down"), false, false, false, false)
        );
        assert_eq!(
            spec(AjAction::ChatScrollToTop),
            chord(ChordKey::Named("home"), false, false, false, false)
        );
        assert_eq!(
            spec(AjAction::ChatScrollToBottom),
            chord(ChordKey::Named("end"), false, false, false, false)
        );
        assert_eq!(
            spec(AjAction::TranscriptFocus),
            chord(ChordKey::Named("tab"), false, false, false, false)
        );
        assert_eq!(
            spec(AjAction::CopyMessage),
            chord(ChordKey::Char('y'), false, false, false, false)
        );
    }

    /// Every default global binding is capture-phase. Transcript-focus (bound
    /// to Tab) is capture-phase too, gated to the autocomplete popup being
    /// closed by its frontend predicate rather than by phase.
    #[test]
    fn every_default_binding_is_capture_phase() {
        for binding in default_global_bindings() {
            assert_eq!(
                binding.phase,
                ChordPhase::Capture,
                "unexpected phase for {:?}",
                binding.action
            );
        }
    }

    #[test]
    fn parses_named_keys_and_aliases() {
        assert_eq!(
            parse_chord("shift+enter").unwrap(),
            chord(ChordKey::Named("enter"), false, false, true, false)
        );
        assert_eq!(parse_chord("return").unwrap().key, ChordKey::Named("enter"));
        assert_eq!(parse_chord("esc").unwrap().key, ChordKey::Named("escape"));
        assert_eq!(
            parse_chord("pageup").unwrap().key,
            ChordKey::Named("page_up")
        );
        assert_eq!(
            parse_chord("pagedown").unwrap().key,
            ChordKey::Named("page_down")
        );
        assert_eq!(parse_chord("space").unwrap().key, ChordKey::Char(' '));
        assert_eq!(parse_chord("f1").unwrap().key, ChordKey::F(1));
        assert_eq!(parse_chord("ctrl+f12").unwrap().key, ChordKey::F(12));
        assert_eq!(
            parse_chord("super+k").unwrap(),
            chord(ChordKey::Char('k'), false, false, false, true)
        );
    }

    #[test]
    fn parses_a_literal_plus_key() {
        assert_eq!(
            parse_chord("ctrl++").unwrap(),
            chord(ChordKey::Char('+'), true, false, false, false)
        );
        assert_eq!(
            parse_chord("ctrl+alt++").unwrap(),
            chord(ChordKey::Char('+'), true, true, false, false)
        );
    }

    #[test]
    fn folds_input_to_lowercase() {
        assert_eq!(
            parse_chord("Ctrl+Shift+P").unwrap(),
            chord(ChordKey::Char('p'), true, false, true, false)
        );
    }

    #[test]
    fn rejects_unknown_modifiers_and_malformed_keys() {
        // Fail-closed on unknown modifiers, meta/hyper included.
        assert_eq!(parse_chord("meta+k"), None);
        assert_eq!(parse_chord("hyper+a"), None);
        assert_eq!(parse_chord("cmd+k"), None);
        assert_eq!(parse_chord("superr+x"), None);
        // Malformed keys.
        assert_eq!(parse_chord(""), None);
        assert_eq!(parse_chord("ab"), None);
        assert_eq!(parse_chord("f0"), None);
        assert_eq!(parse_chord("f99"), None);
        // Unknown modifier on a literal `+` key.
        assert_eq!(parse_chord("meta++"), None);
    }
}
