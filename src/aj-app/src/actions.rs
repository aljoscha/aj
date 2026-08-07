//! The typed keymap data layer: the global action vocabulary, the chord
//! grammar parser, the default binding table, and the merge of the user's
//! `[keybindings]` overrides onto it ([`install_keybindings`]).
//!
//! This module is the frontend-neutral half of the keymap boundary. It
//! knows the `aj.*` action IDs and their default chords (from
//! [`crate::keybindings::AJ_KEYBINDINGS`]), validates and installs any user
//! overrides, and compiles the effective bindings into [`ChordSpec`]s, a
//! representation with no key-event types in it. A frontend translates a
//! `ChordSpec` into its own activator type (the vaxis frontend maps
//! [`ChordKey::Named`] names through vaxis's key-name table) and attaches its
//! own context predicates and handlers.
//!
//! Only globally-dispatched actions appear here. The overlay-local
//! bindings in `AJ_KEYBINDINGS` (`aj.history.toggle_scope`,
//! `aj.agent.toggle_scope`, `aj.task.kill`, `aj.settings.clear`,
//! `aj.usage.reset`) are handled at-target by the focused overlay
//! widget, so they are that widget's data, not part of the global
//! action vocabulary.

use std::collections::BTreeMap;
use std::fmt;

use crate::keybindings::{
    ACTION_AGENT_PICKER, ACTION_BRANCH_MESSAGE, ACTION_CHAT_PAGE_DOWN, ACTION_CHAT_PAGE_UP,
    ACTION_CHAT_SCROLL_BOTTOM, ACTION_CHAT_SCROLL_TOP, ACTION_CLIPBOARD_PASTE_IMAGE,
    ACTION_COPY_MESSAGE, ACTION_DEQUEUE, ACTION_HISTORY_OPEN, ACTION_OVERLAY_CLOSE_ALL,
    ACTION_PALETTE_OPEN, ACTION_SESSION_NEW, ACTION_SESSION_NEXT, ACTION_SESSION_PREV,
    ACTION_SESSION_TAG, ACTION_SIDEBAR_TOGGLE, ACTION_SUBMIT_STEERING, ACTION_THINKING_TOGGLE,
    ACTION_TOOLS_EXPAND, ACTION_TRANSCRIPT_FOCUS, AJ_KEYBINDINGS, default_chord, effective_chord,
    set_overrides,
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
    /// Branch the conversation from the focused user message, live only in
    /// transcript-focus mode (`aj.transcript.branch_message`).
    BranchMessage,
    /// Show or hide the session sidebar (`aj.sidebar.toggle`).
    SidebarToggle,
    /// Focus the next session in the sidebar's order (`aj.session.next`).
    SessionNext,
    /// Focus the previous session in the sidebar's order (`aj.session.prev`).
    SessionPrev,
    /// Create a session on this client's peer and focus it (`aj.session.new`).
    SessionNew,
    /// Edit the focused session's tag (`aj.session.tag`).
    SessionTag,
    /// Cancel the viewed agent's running turn (the Ctrl+C ladder's first
    /// rung).
    CancelTurn,
    /// Quit the application (the Ctrl+C Ctrl+C ladder's second rung).
    Quit,
}

impl AjAction {
    /// The `aj.*` action-ID string for this action, or `None` for the fixed
    /// Ctrl+C ladder rungs (`CancelTurn`, `Quit`), which have no
    /// [`crate::keybindings::AJ_KEYBINDINGS`] row.
    pub fn action_id(self) -> Option<&'static str> {
        Some(match self {
            AjAction::ThinkingToggle => ACTION_THINKING_TOGGLE,
            AjAction::ToolsExpand => ACTION_TOOLS_EXPAND,
            AjAction::PasteImage => ACTION_CLIPBOARD_PASTE_IMAGE,
            AjAction::PaletteOpen => ACTION_PALETTE_OPEN,
            AjAction::CloseAllOverlays => ACTION_OVERLAY_CLOSE_ALL,
            AjAction::HistoryOpen => ACTION_HISTORY_OPEN,
            AjAction::AgentPickerOpen => ACTION_AGENT_PICKER,
            AjAction::SidebarToggle => ACTION_SIDEBAR_TOGGLE,
            AjAction::SessionNext => ACTION_SESSION_NEXT,
            AjAction::SessionPrev => ACTION_SESSION_PREV,
            AjAction::SessionNew => ACTION_SESSION_NEW,
            AjAction::SessionTag => ACTION_SESSION_TAG,
            AjAction::Steer => ACTION_SUBMIT_STEERING,
            AjAction::Dequeue => ACTION_DEQUEUE,
            AjAction::ChatPageUp => ACTION_CHAT_PAGE_UP,
            AjAction::ChatPageDown => ACTION_CHAT_PAGE_DOWN,
            AjAction::ChatScrollToTop => ACTION_CHAT_SCROLL_TOP,
            AjAction::ChatScrollToBottom => ACTION_CHAT_SCROLL_BOTTOM,
            AjAction::TranscriptFocus => ACTION_TRANSCRIPT_FOCUS,
            AjAction::CopyMessage => ACTION_COPY_MESSAGE,
            AjAction::BranchMessage => ACTION_BRANCH_MESSAGE,
            AjAction::CancelTurn | AjAction::Quit => return None,
        })
    }
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

/// Characters that follow an `ESC` as the introducer of an escape sequence
/// (`SS3`, `DCS`, `SOS`, `CSI`, `OSC`, `PM`, `APC`).
const ESCAPE_INTRODUCERS: &[char] = &['O', 'P', 'X', '[', ']', '^', '_'];

/// The control codes `0x1C..=0x1F` name no key, so the input parser decodes
/// them as text rather than as a ctrl chord.
const CTRL_ARRIVES_AS_TEXT: &str =
    "the control code the terminal sends for it arrives as text, not as a ctrl chord";

/// Ctrl chords whose control code belongs to another key. The reason names the
/// key the keystroke arrives as, which is what the user observes.
const CTRL_ALIASES: &[(char, &str)] = &[
    ('h', "the terminal sends the backspace code for it"),
    ('i', "the terminal sends the tab code for it"),
    ('m', "the terminal sends the enter code for it"),
    ('[', "the terminal sends the escape code for it"),
    ('?', "the terminal sends the backspace code for it"),
    (' ', "the terminal sends the ctrl+@ code for it"),
    ('\\', CTRL_ARRIVES_AS_TEXT),
    (']', CTRL_ARRIVES_AS_TEXT),
    ('^', CTRL_ARRIVES_AS_TEXT),
    ('_', CTRL_ARRIVES_AS_TEXT),
];

/// Named keys a terminal sends as a single control code instead of an escape
/// sequence.
const CONTROL_CODE_KEYS: &[&str] = &["enter", "escape", "tab", "backspace"];

/// Why no keystroke can produce `spec`, or `None` when a terminal can send it.
///
/// A chord only works if the bytes a terminal transmits for it decode back into
/// the key the keymap matches on, and the terminal has far fewer encodings than
/// the chord grammar has spellings. Two shapes exist. A cursor, editing, or
/// function key arrives as a CSI sequence carrying a modifier parameter, which
/// encodes shift, alt, ctrl, and super exactly, so every combination survives.
/// A character or a control-code key arrives as its own bytes with at most an
/// `ESC` prefix for alt, which has nowhere to put a modifier, so most
/// combinations on those either collide with another key or are simply not sent.
///
/// The kitty keyboard protocol's `CSI u` form does encode every key and modifier
/// exactly, but a terminal only speaks it after negotiating the capability. A
/// chord that needs it would work on some terminals and fire the wrong action on
/// the rest, so it does not count as typeable here.
///
/// The returned reason completes the sentence "ignored keybinding X for Y:
/// `<reason>`".
///
/// These rules restate what the terminal input parser does. That parser lives in
/// the TUI crate, which this crate may not depend on, so the frontend carries a
/// test sweeping the whole chord space through the real parser and asserting it
/// agrees with this function.
pub fn untypeable_reason(spec: &ChordSpec) -> Option<&'static str> {
    match spec.key {
        ChordKey::Char(c) => char_reason(c, spec),
        ChordKey::Named(name) => named_reason(name, spec),
        // Function keys arrive as the numbered CSI form. xterm's numbering runs
        // out at F20, and nothing above that has an encoding to recognize.
        ChordKey::F(n) => (!(1..=20).contains(&n))
            .then_some("no terminal has an encoding for function keys above f20"),
    }
}

fn char_reason(c: char, spec: &ChordSpec) -> Option<&'static str> {
    if c.is_control() {
        return Some(
            "a bare control character is not a key, it is how a ctrl chord or a named key is sent",
        );
    }
    // (ctrl, alt, shift, super)
    match (spec.ctrl, spec.alt, spec.shift, spec.super_mod) {
        (false, false, false, false) => None,
        // Shift is not transmitted next to a character, the terminal sends the
        // shifted glyph on its own. Only a letter's shifted glyph reads back as
        // a shift chord, every other one depends on the keyboard layout.
        (false, false, true, false) => (!c.is_ascii_alphabetic()).then_some(
            "a terminal sends the shifted glyph rather than shift plus the key, which only \
             reads back as a shift chord for a letter",
        ),
        (true, false, false, false) => ctrl_reason(c),
        (false, true, false, false) => alt_reason(c),
        _ => Some(
            "a terminal sends a character key as its own bytes, optionally ESC-prefixed for \
             alt, which carries no second modifier and never super",
        ),
    }
}

fn ctrl_reason(c: char) -> Option<&'static str> {
    if let Some((_, reason)) = CTRL_ALIASES.iter().find(|(key, _)| *key == c) {
        return Some(reason);
    }
    // The conventional control codes are the lowercase letters plus `@`, and
    // `CTRL_ALIASES` has already taken the ones another key owns.
    if c.is_ascii_lowercase() || c == '@' {
        return None;
    }
    Some("a terminal has no control code for this key, so ctrl never reaches the application")
}

fn alt_reason(c: char) -> Option<&'static str> {
    if ESCAPE_INTRODUCERS.contains(&c) {
        return Some(
            "alt is sent as an ESC prefix and this character introduces an escape sequence, \
             so a terminal cannot tell the two apart",
        );
    }
    if !c.is_ascii() {
        return Some(
            "alt is sent as an ESC prefix, which carries only the first byte of a multi-byte \
             character",
        );
    }
    None
}

fn named_reason(name: &str, spec: &ChordSpec) -> Option<&'static str> {
    // Everything else is a cursor or editing key, which arrives as a CSI
    // sequence whose modifier parameter carries every combination.
    if !CONTROL_CODE_KEYS.contains(&name) {
        return None;
    }
    // (ctrl, alt, shift, super)
    match (spec.ctrl, spec.alt, spec.shift, spec.super_mod) {
        (false, false, false, false) | (false, true, false, false) => None,
        // `CSI Z` is the one modified form of a control-code key that terminals
        // send, and it exists for shift+tab alone.
        (false, false, true, false) if name == "tab" => None,
        _ => Some(
            "a terminal sends this key as a bare control code, which carries no modifier \
             beyond the ESC prefix that means alt",
        ),
    }
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

/// The global bindings actually in effect, compiled from the `AJ_KEYBINDINGS`
/// chord strings (via [`effective_chord`], so a user override wins over the
/// default) so the data can't drift apart.
///
/// Every entry is capture-phase: these are the chords `aj` intercepts before
/// its editor sees the keystroke. [`AjAction::TranscriptFocus`] (bound to
/// `tab`) is capture-phase too, but its frontend predicate gates it to the
/// autocomplete popup being closed, so Tab focuses the transcript even with a
/// draft, and only an open popup keeps Tab for the editor to apply a completion
/// (Spec E section 1). The Ctrl+C ladder (`CancelTurn`, `Quit`) is not in the
/// table, see [`AjAction`].
pub fn global_bindings() -> Vec<GlobalBinding> {
    let compiled = |action: AjAction, action_id: &str, phase: ChordPhase| {
        let chord = effective_chord(action_id).expect("every global action resolves a chord");
        GlobalBinding {
            action,
            chord: parse_chord(chord).expect("the effective chords parse"),
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
        compiled(AjAction::SidebarToggle, ACTION_SIDEBAR_TOGGLE, Capture),
        compiled(AjAction::SessionNext, ACTION_SESSION_NEXT, Capture),
        compiled(AjAction::SessionPrev, ACTION_SESSION_PREV, Capture),
        compiled(AjAction::SessionNew, ACTION_SESSION_NEW, Capture),
        compiled(AjAction::SessionTag, ACTION_SESSION_TAG, Capture),
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
        compiled(AjAction::BranchMessage, ACTION_BRANCH_MESSAGE, Capture),
    ]
}

/// A user keybinding override that was rejected, surfaced as a startup warning
/// so the user learns their config line had no effect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeybindingProblem {
    /// The config key is not a known `aj.*` action id.
    UnknownAction { action: String },
    /// The chord string does not parse (see [`parse_chord`]).
    InvalidChord { action: String, chord: String },
    /// The chord parses but no keystroke produces it, so the binding would be
    /// dead or would fire on a different key (see [`untypeable_reason`]).
    Untypeable {
        action: String,
        chord: String,
        reason: &'static str,
    },
    /// The chord clashes with a reserved key or another global binding, so the
    /// action keeps its default. `with` names what it clashed with.
    Conflict {
        action: String,
        chord: String,
        with: String,
    },
}

impl fmt::Display for KeybindingProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeybindingProblem::UnknownAction { action } => {
                write!(f, "ignored keybinding for unknown action {action:?}")
            }
            KeybindingProblem::InvalidChord { action, chord } => write!(
                f,
                "ignored keybinding {chord:?} for {action:?}: not a valid chord"
            ),
            KeybindingProblem::Untypeable {
                action,
                chord,
                reason,
            } => write!(
                f,
                "ignored keybinding {chord:?} for {action:?}: no keystroke produces it, {reason}"
            ),
            KeybindingProblem::Conflict {
                action,
                chord,
                with,
            } => write!(
                f,
                "ignored keybinding {chord:?} for {action:?}: conflicts with {with}, using the default"
            ),
        }
    }
}

/// Chords reserved for fixed terminal conventions, which an override may not
/// claim (see [`crate::keybindings::fixed_keys`]): the Ctrl+C ladder and the
/// login dialog's Ctrl+Y copy.
const RESERVED_CHORDS: &[&str] = &["ctrl+c", "ctrl+y"];

/// The global actions the frontend binds with an always-true predicate, so they
/// fire regardless of overlay or focus state. Because they pre-empt at-target
/// dispatch even while an overlay is up, an overlay-local override may not land
/// on one of their effective chords, or it would be silently shadowed.
///
/// This mirrors the frontend keymap's predicates. The frontend has a drift
/// guard asserting the two stay in sync.
pub const ALWAYS_ON_ACTION_IDS: &[&str] = &[
    ACTION_THINKING_TOGGLE,
    ACTION_TOOLS_EXPAND,
    ACTION_CLIPBOARD_PASTE_IMAGE,
];

/// The canonical `&'static str` action id matching `key`, or `None` when `key`
/// names no known `aj.*` action.
fn known_action_id(key: &str) -> Option<&'static str> {
    AJ_KEYBINDINGS
        .iter()
        .map(|(id, _, _)| *id)
        .find(|id| *id == key)
}

/// Of two colliding actions, the one whose override to drop. An action sitting
/// on its built-in default cannot be dropped, so we blame whichever is an
/// accepted override, and the later-sorting id when both are. At least one is
/// always an override, since the built-in defaults are collision-free.
fn blame_of(
    a: &'static str,
    b: &'static str,
    accepted: &BTreeMap<&'static str, ChordSpec>,
) -> &'static str {
    match (accepted.contains_key(a), accepted.contains_key(b)) {
        (true, true) => a.max(b),
        (true, false) => a,
        (false, true) => b,
        (false, false) => unreachable!("built-in defaults are collision-free"),
    }
}

/// Validate the user's `[keybindings]` overrides and install the accepted set
/// into the process-global store [`effective_chord`] reads.
///
/// Each entry is `(action_id, chord)`. An entry is rejected, and reported as a
/// [`KeybindingProblem`], when its action is unknown, its chord does not parse,
/// no keystroke can produce its chord (see [`untypeable_reason`]), or its chord
/// is reserved. Beyond that, the accepted set must leave the final bindings
/// collision-free: the global chords stay mutually distinct, and an
/// overlay-local chord may not equal an always-on global chord (those fire even
/// while an overlay is up, so they would shadow it). An override that breaks
/// that is dropped with a [`KeybindingProblem::Conflict`] and its action keeps
/// its built-in default. Accepted entries replace (not extend) their action's
/// default. Restating an action's own default is a silent no-op. Call once at
/// startup, before the keymap is built or any hint renders.
///
/// Collisions are judged on the final assignment, not on intermediate states,
/// so a swap (two actions trading chords) or a longer chain (one rebind freeing
/// the chord another wants) is accepted whole. Only a genuine clash, two
/// actions left on the same chord, is rejected, and then the later-sorting
/// action id is the one dropped.
pub fn install_keybindings<I>(overrides: I) -> Vec<KeybindingProblem>
where
    I: IntoIterator<Item = (String, String)>,
{
    let reserved: Vec<ChordSpec> = RESERVED_CHORDS
        .iter()
        .filter_map(|c| parse_chord(c))
        .collect();
    let global_ids: Vec<&'static str> = global_bindings()
        .iter()
        .filter_map(|b| b.action.action_id())
        .collect();
    let overlay_local_ids: Vec<&'static str> = AJ_KEYBINDINGS
        .iter()
        .map(|(id, _, _)| *id)
        .filter(|id| !global_ids.contains(id))
        .collect();

    // Phase 0: validate each entry on its own. Survivors are real moves off the
    // default; unknown / unparseable / reserved / no-op entries are settled here
    // so the collision pass only weighs genuine rebinds.
    let mut candidates: Vec<(&'static str, ChordSpec, String)> = Vec::new();
    let mut problems = Vec::new();
    for (key, chord) in overrides {
        let Some(action_id) = known_action_id(&key) else {
            problems.push(KeybindingProblem::UnknownAction { action: key });
            continue;
        };
        let Some(spec) = parse_chord(&chord) else {
            problems.push(KeybindingProblem::InvalidChord { action: key, chord });
            continue;
        };
        // Restating an action's own default is a silent no-op: leaving it out of
        // the store lets `effective_chord` return the default, and skips the
        // reserved/collision checks, which would otherwise flag the one default
        // that is a reserved chord (close-all's ctrl+c).
        if default_chord(action_id).and_then(parse_chord) == Some(spec) {
            continue;
        }
        // A chord no terminal can send is worse than no override at all: the
        // action goes dead, and the keystroke the user pressed often reaches a
        // different binding instead.
        if let Some(reason) = untypeable_reason(&spec) {
            problems.push(KeybindingProblem::Untypeable {
                action: key,
                chord,
                reason,
            });
            continue;
        }
        if reserved.contains(&spec) {
            problems.push(KeybindingProblem::Conflict {
                action: key,
                chord,
                with: "a reserved key".to_string(),
            });
            continue;
        }
        candidates.push((action_id, spec, chord));
    }

    // Phase 1: resolve collisions on the *final* assignment (accepted override
    // else default), not on the intermediate states a greedy pass would see, so
    // a swap or chain of rebinds that ends collision-free is accepted whole. We
    // start optimistic with every candidate accepted, then drop one offender at
    // a time until the assignment is clean. This terminates because each step
    // removes an override and the all-defaults floor is collision-free (see
    // `defaults_are_collision_free`).
    let mut accepted: BTreeMap<&'static str, ChordSpec> = candidates
        .iter()
        .map(|(id, spec, _)| (*id, *spec))
        .collect();

    loop {
        let offender: Option<(&'static str, String)> = {
            let chord_of = |id: &str| -> ChordSpec {
                accepted.get(id).copied().unwrap_or_else(|| {
                    parse_chord(default_chord(id).expect("known action id"))
                        .expect("built-in default parses")
                })
            };

            let mut found: Option<(&'static str, String)> = None;
            // C1: two global actions may not share a chord.
            'c1: for (i, a) in global_ids.iter().enumerate() {
                for b in &global_ids[i + 1..] {
                    if chord_of(a) == chord_of(b) {
                        let blame = blame_of(a, b, &accepted);
                        let other = if blame == *a { *b } else { *a };
                        found = Some((blame, format!("the binding for {other:?}")));
                        break 'c1;
                    }
                }
            }
            // C2: an overlay-local chord may not equal an always-on global's.
            if found.is_none() {
                'c2: for o in &overlay_local_ids {
                    for g in ALWAYS_ON_ACTION_IDS {
                        if chord_of(o) == chord_of(g) {
                            let blame = if accepted.contains_key(*o) { *o } else { *g };
                            let other = if blame == *o { *g } else { *o };
                            found = Some((blame, format!("the binding for {other:?}")));
                            break 'c2;
                        }
                    }
                }
            }
            found
        };

        let Some((blame, with)) = offender else { break };
        accepted.remove(blame);
        let chord = candidates
            .iter()
            .find(|(id, _, _)| *id == blame)
            .map(|(_, _, chord)| chord.clone())
            .expect("a blamed action is an accepted candidate");
        problems.push(KeybindingProblem::Conflict {
            action: blame.to_string(),
            chord,
            with,
        });
    }

    // Leak the lowercased canonical chord of each surviving candidate so the
    // store hands back `&'static str`, the same borrow `default_chord` yields.
    let mut store: BTreeMap<&'static str, &'static str> = BTreeMap::new();
    for (id, _, chord) in &candidates {
        if accepted.contains_key(id) {
            let leaked: &'static str = Box::leak(chord.to_ascii_lowercase().into_boxed_str());
            store.insert(id, leaked);
        }
    }
    set_overrides(store);
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keybindings::{AJ_KEYBINDINGS, STORE_TEST_GUARD};

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
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_overrides(BTreeMap::new());
        let bindings = global_bindings();
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
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set_overrides(BTreeMap::new());
        for binding in global_bindings() {
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

    /// A valid override is installed and wins, while an unknown action, an
    /// unparseable chord, and a reserved key are each rejected with a matching
    /// problem and leave the action on its default.
    #[test]
    fn install_keybindings_accepts_valid_and_reports_bad() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let problems = install_keybindings([
            ("aj.palette.open".to_string(), "alt+p".to_string()),
            ("aj.not.a.thing".to_string(), "ctrl+z".to_string()),
            ("aj.tools.expand".to_string(), "bogus".to_string()),
            ("aj.thinking.toggle".to_string(), "ctrl+c".to_string()),
        ]);

        assert_eq!(
            effective_chord("aj.palette.open"),
            Some("alt+p"),
            "the valid override took effect"
        );
        assert_eq!(effective_chord("aj.tools.expand"), Some("alt+o"));
        assert_eq!(effective_chord("aj.thinking.toggle"), Some("alt+t"));

        assert!(problems.iter().any(|p| matches!(
            p,
            KeybindingProblem::UnknownAction { action } if action == "aj.not.a.thing"
        )));
        assert!(problems.iter().any(|p| matches!(
            p,
            KeybindingProblem::InvalidChord { action, .. } if action == "aj.tools.expand"
        )));
        assert!(problems.iter().any(|p| matches!(
            p,
            KeybindingProblem::Conflict { action, .. } if action == "aj.thinking.toggle"
        )));

        set_overrides(BTreeMap::new());
    }

    /// An override colliding with another global binding is rejected as a
    /// conflict, leaving the action on its default.
    #[test]
    fn install_keybindings_rejects_a_global_collision() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // alt+a is the agent-picker default, so binding the palette to it clashes.
        let problems = install_keybindings([("aj.palette.open".to_string(), "alt+a".to_string())]);

        assert_eq!(
            effective_chord("aj.palette.open"),
            Some("ctrl+o"),
            "the palette kept its default after the conflict"
        );
        assert!(problems.iter().any(|p| matches!(
            p,
            KeybindingProblem::Conflict { action, .. } if action == "aj.palette.open"
        )));

        set_overrides(BTreeMap::new());
    }

    /// Overlay-local actions are matched at-target and may share a chord (the
    /// built-in table already does with the two toggle-scope actions), so the
    /// collision check leaves them alone.
    #[test]
    fn install_keybindings_allows_overlay_local_actions_to_share_a_chord() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let problems = install_keybindings([
            ("aj.usage.reset".to_string(), "x".to_string()),
            ("aj.task.kill".to_string(), "x".to_string()),
        ]);

        assert_eq!(effective_chord("aj.usage.reset"), Some("x"));
        assert_eq!(effective_chord("aj.task.kill"), Some("x"));
        assert!(
            problems.is_empty(),
            "overlay-local sharing is allowed: {problems:?}"
        );

        set_overrides(BTreeMap::new());
    }

    /// Two overrides landing on the same chord clash. The collision keeps the
    /// earlier-sorting action id and drops the other, leaving it on its default.
    #[test]
    fn install_keybindings_rejects_a_clash_with_an_earlier_override() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Both target alt+p (no default uses it). `aj.agent.open` sorts
        // before `aj.palette.open`, so palette is the later id and gets dropped.
        let problems = install_keybindings([
            ("aj.agent.open".to_string(), "alt+p".to_string()),
            ("aj.palette.open".to_string(), "alt+p".to_string()),
        ]);

        assert_eq!(effective_chord("aj.agent.open"), Some("alt+p"));
        assert_eq!(
            effective_chord("aj.palette.open"),
            Some("ctrl+o"),
            "the later-sorting override was dropped back to its default"
        );
        assert!(problems.iter().any(|p| matches!(
            p,
            KeybindingProblem::Conflict { action, .. } if action == "aj.palette.open"
        )));

        set_overrides(BTreeMap::new());
    }

    /// A user override that no keystroke can produce is refused, so the action
    /// keeps a chord that works instead of going silently dead. Each case here
    /// is a different hazard: a ctrl chord whose control code belongs to Tab, an
    /// alt chord whose character introduces an escape sequence, a modifier
    /// combination a character key cannot carry, and a function key above the
    /// decoded range.
    #[test]
    fn install_keybindings_refuses_untypeable_chords() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let problems = install_keybindings([
            ("aj.palette.open".to_string(), "ctrl+i".to_string()),
            ("aj.agent.open".to_string(), "alt+]".to_string()),
            ("aj.tools.expand".to_string(), "alt+shift+x".to_string()),
            ("aj.history.open".to_string(), "f21".to_string()),
        ]);

        assert_eq!(effective_chord("aj.palette.open"), Some("ctrl+o"));
        assert_eq!(effective_chord("aj.agent.open"), Some("alt+a"));
        assert_eq!(effective_chord("aj.tools.expand"), Some("alt+o"));
        assert_eq!(effective_chord("aj.history.open"), Some("ctrl+r"));

        let refused: Vec<&str> = problems
            .iter()
            .filter_map(|p| match p {
                KeybindingProblem::Untypeable { chord, .. } => Some(chord.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(refused, ["ctrl+i", "alt+]", "alt+shift+x", "f21"]);
        assert!(
            problems[0]
                .to_string()
                .contains("the terminal sends the tab code"),
            "the message names the key the keystroke arrives as: {}",
            problems[0]
        );

        set_overrides(BTreeMap::new());
    }

    /// Every default chord is typeable. The frontend proves this against the
    /// real input parser as well, but keeping it here means a table edit fails
    /// in the crate that owns the table.
    #[test]
    fn every_default_chord_is_typeable() {
        for (action_id, chord, _) in AJ_KEYBINDINGS {
            let spec = parse_chord(chord).expect("default chords parse");
            assert_eq!(
                untypeable_reason(&spec),
                None,
                "default chord {chord:?} for {action_id} cannot be typed"
            );
        }
    }

    /// Both reserved chords (the Ctrl+C ladder and the login Ctrl+Y copy) are
    /// rejected for any action, with the reason pinned so a regression that
    /// changed reserved handling to some other conflict reason is caught.
    #[test]
    fn install_keybindings_rejects_reserved_chords() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let problems = install_keybindings([("aj.palette.open".to_string(), "ctrl+y".to_string())]);

        assert_eq!(effective_chord("aj.palette.open"), Some("ctrl+o"));
        assert!(problems.iter().any(|p| matches!(
            p,
            KeybindingProblem::Conflict { action, with, .. }
                if action == "aj.palette.open" && with == "a reserved key"
        )));

        set_overrides(BTreeMap::new());
    }

    /// Restating an action's own default is a silent no-op, even for the one
    /// default that is a reserved chord (close-all's ctrl+c): no problem, and
    /// the action keeps its default.
    #[test]
    fn install_keybindings_treats_restating_the_default_as_a_noop() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let problems = install_keybindings([
            ("aj.overlay.close_all".to_string(), "ctrl+c".to_string()),
            ("aj.palette.open".to_string(), "ctrl+o".to_string()),
        ]);

        assert!(
            problems.is_empty(),
            "restating defaults is a no-op: {problems:?}"
        );
        assert_eq!(effective_chord("aj.overlay.close_all"), Some("ctrl+c"));
        assert_eq!(effective_chord("aj.palette.open"), Some("ctrl+o"));

        set_overrides(BTreeMap::new());
    }

    /// Two global actions trading chords (a swap) is collision-free in the final
    /// assignment, so both overrides are accepted. This is the case the old
    /// greedy pass rejected.
    #[test]
    fn install_keybindings_accepts_a_swap() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        // Palette (default ctrl+o) and agent (default alt+a) trade chords.
        let problems = install_keybindings([
            ("aj.palette.open".to_string(), "alt+a".to_string()),
            ("aj.agent.open".to_string(), "ctrl+o".to_string()),
        ]);

        assert!(
            problems.is_empty(),
            "a swap is collision-free: {problems:?}"
        );
        assert_eq!(effective_chord("aj.palette.open"), Some("alt+a"));
        assert_eq!(effective_chord("aj.agent.open"), Some("ctrl+o"));

        set_overrides(BTreeMap::new());
    }

    /// A chain where one rebind frees the chord another wants is accepted whole:
    /// agent takes the palette's default while the palette moves to a free key.
    #[test]
    fn install_keybindings_accepts_a_chain() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let problems = install_keybindings([
            ("aj.agent.open".to_string(), "ctrl+o".to_string()),
            ("aj.palette.open".to_string(), "f5".to_string()),
        ]);

        assert!(
            problems.is_empty(),
            "a chain is collision-free: {problems:?}"
        );
        assert_eq!(effective_chord("aj.agent.open"), Some("ctrl+o"));
        assert_eq!(effective_chord("aj.palette.open"), Some("f5"));

        set_overrides(BTreeMap::new());
    }

    /// An overlay-local override onto an always-on global chord is rejected: the
    /// always-on global (here tools-expand on alt+o) pre-empts at-target
    /// dispatch even inside an overlay, so the overlay-local binding would be
    /// silently shadowed.
    #[test]
    fn install_keybindings_rejects_overlay_local_shadowing_an_always_on_global() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let problems = install_keybindings([("aj.usage.reset".to_string(), "alt+o".to_string())]);

        assert_eq!(
            effective_chord("aj.usage.reset"),
            Some("r"),
            "the overlay-local action kept its default"
        );
        assert!(problems.iter().any(|p| matches!(
            p,
            KeybindingProblem::Conflict { action, with, .. }
                if action == "aj.usage.reset" && with.contains("aj.tools.expand")
        )));

        set_overrides(BTreeMap::new());
    }

    /// Moving an always-on global off its chord frees that chord for an
    /// overlay-local action: the shadow check reads the always-on global's
    /// effective chord, not just its default.
    #[test]
    fn install_keybindings_allows_overlay_local_onto_a_vacated_always_on_chord() {
        let _guard = STORE_TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let problems = install_keybindings([
            ("aj.tools.expand".to_string(), "f6".to_string()),
            ("aj.usage.reset".to_string(), "alt+o".to_string()),
        ]);

        assert!(
            problems.is_empty(),
            "alt+o is free once tools-expand moves: {problems:?}"
        );
        assert_eq!(effective_chord("aj.tools.expand"), Some("f6"));
        assert_eq!(effective_chord("aj.usage.reset"), Some("alt+o"));

        set_overrides(BTreeMap::new());
    }

    /// The built-in defaults satisfy the same constraints the resolver enforces:
    /// the global chords are mutually distinct and no overlay-local default
    /// equals an always-on global default. The resolver's termination relies on
    /// this, since dropping every override reverts to this floor.
    #[test]
    fn defaults_are_collision_free() {
        let global_ids: Vec<&str> = global_bindings()
            .iter()
            .filter_map(|b| b.action.action_id())
            .collect();
        let spec = |id: &str| parse_chord(default_chord(id).expect("known id")).expect("parses");

        for (i, a) in global_ids.iter().enumerate() {
            for b in &global_ids[i + 1..] {
                assert_ne!(spec(a), spec(b), "global default collision: {a} and {b}");
            }
        }

        let overlay_local: Vec<&str> = AJ_KEYBINDINGS
            .iter()
            .map(|(id, _, _)| *id)
            .filter(|id| !global_ids.contains(id))
            .collect();
        for o in &overlay_local {
            for g in ALWAYS_ON_ACTION_IDS {
                assert_ne!(spec(o), spec(g), "overlay-local {o} shadows always-on {g}");
            }
        }
    }
}
