//! Compiles the shared keybinding data (`aj_app::actions`) into the vxfw
//! keymap engine.
//!
//! This is the frontend half of the keymap boundary: `aj-app` owns the
//! action vocabulary and the parsed default chords, this module turns
//! them into vaxis [`Activator`]s, attaches the context predicates, and
//! adds the fixed Ctrl+C ladder. The [`crate::interactive::Shell`] wires
//! the resulting [`Keymap`] into a `KeymapController` wrapping its base
//! layout.

use std::cell::RefCell;
use std::rc::Rc;

use aj_agent::events::AgentId;
use aj_app::actions::{AjAction, ChordKey, ChordPhase, ChordSpec, global_bindings};
use aj_app::chat::ChatState;
use vaxis::key::{Key, Modifiers, name_map};
use vaxis::vxfw::{Activator, BindingPhase, Entry, Keymap, TextArea};

use crate::overlay::OverlayStack;

/// The host state the keymap's context predicates read.
///
/// Overlay liveness is read straight off the shared stack, so it is
/// current even for mutations made mid-dispatch (a palette opened by
/// one key gates the very next one). `turn_running` is a mirror the
/// drive loop refreshes at its per-iteration sync point, because the
/// turn bookkeeping lives on the host's `World`, which widgets can't
/// reach. The drive loop is its single writer.
pub(crate) struct HostCtx {
    pub(crate) overlays: Rc<RefCell<OverlayStack>>,
    /// The compose editor, shared with the base layout. Read live (like
    /// `overlays`) so its autocomplete state is current at match time, no
    /// mirror. The transcript-focus chord gates on the autocomplete popup being
    /// closed (see [`focus_enabled`]).
    pub(crate) editor: Rc<RefCell<TextArea>>,
    /// Whether the transcript is in focus mode, the same cell the
    /// [`crate::transcript::TranscriptView`] writes (its `FocusIn`/`FocusOut`
    /// are the single writer). Read live so the copy chord is gated on the
    /// current mode: `y` is captured only while the transcript is focused, so
    /// with the editor focused it types normally (see [`in_transcript_focus`]).
    pub(crate) focus_mode: Rc<std::cell::Cell<bool>>,
    /// Whether the viewed agent is busy (a binary-driven turn or a
    /// running initial sub-agent spawn), i.e. whether Ctrl+C has
    /// something to cancel.
    pub(crate) turn_running: bool,
    /// Whether an OAuth login dialog is up. It is a modal like the other
    /// overlays, but its own Esc/Ctrl+C handling flips a cancel flag the
    /// drive loop polls, so the close-all chord must not pre-empt it.
    /// The drive loop is this field's single writer.
    pub(crate) login_active: bool,
    /// The chat model, read live by the recall gesture (plain Up / Ctrl+P) so
    /// it sees the pending message the model currently holds. Shared with the
    /// widgets and the drive loop, which is the model's single writer, so
    /// there is no mirror to go stale.
    pub(crate) chat: Rc<RefCell<ChatState>>,
    /// The agent whose transcript is in view, keying the queue lookup in
    /// [`Self::chat`]. A mirror the drive loop refreshes at its sync point,
    /// like `turn_running`.
    pub(crate) active_view: AgentId,
}

fn overlay_open(cx: &HostCtx) -> bool {
    cx.overlays.borrow().is_open()
}

fn no_overlay(cx: &HostCtx) -> bool {
    !overlay_open(cx)
}

/// Transcript focus is entered with Tab whenever the autocomplete popup is
/// closed: the chord matches in the capture phase, ahead of the editor, so
/// gating it on the popup lets Tab stay the editor's accept key while the popup
/// is open, and focus the transcript otherwise, draft text and all (Spec E
/// section 1). Reading the editor here is safe even though it is a focused
/// widget: the capture-phase match runs at the root before the event descends
/// to the editor, so the editor is not already borrowed.
fn focus_enabled(cx: &HostCtx) -> bool {
    no_overlay(cx) && !cx.editor.borrow().is_showing_autocomplete()
}

/// The copy chord is live only while the transcript is focused. Gating it here
/// (not by phase) is what keeps `y` an ordinary character in the editor: with
/// the editor focused the flag is false, so the capture-phase binding declines
/// and the key descends to the editor. When an overlay opens it steals focus
/// from the transcript, whose `FocusOut` clears the flag, so this is already
/// false under overlays.
fn in_transcript_focus(cx: &HostCtx) -> bool {
    cx.focus_mode.get()
}

/// The editor owns base-layout focus when no overlay or transcript has it.
fn in_editor_focus(cx: &HostCtx) -> bool {
    no_overlay(cx) && !in_transcript_focus(cx)
}

fn can_cancel(cx: &HostCtx) -> bool {
    no_overlay(cx) && cx.turn_running
}

fn can_arm_quit(cx: &HostCtx) -> bool {
    no_overlay(cx) && !cx.turn_running
}

/// The plain Up / Ctrl+P recall gesture: with the editor empty and a message
/// pending for the active view, these keys yank the pending message into the
/// editor rather than navigating history (mirroring `aj`). Stricter than the
/// `alt+up` dequeue gate ([`no_overlay`], fires regardless of editor contents),
/// so with any draft in the editor the key falls through to the editor's own
/// history / cursor nav. Reading the editor here is safe for the same reason
/// [`focus_enabled`] gives: the capture-phase match runs at the root before the
/// event descends to the editor, so the editor is not already borrowed. The
/// chat borrow is safe for the same reason: nothing below the root holds one
/// during dispatch.
///
/// Also gated off while the transcript is focused: there Up / Ctrl+P step
/// through the user messages (`handle_focus_key`), and that stepping is
/// bubble-phase, so without this gate the capture-phase recall would pre-empt
/// it. Declining here lets the key descend to the transcript's focus stepping.
fn can_recall_pending(cx: &HostCtx) -> bool {
    in_editor_focus(cx)
        && cx.editor.borrow().text().is_empty()
        && crate::pending::pending_message(&cx.chat.borrow(), cx.active_view).is_some()
}

/// Close-all is inert while a login dialog is up: the dialog owns its own
/// Esc/Ctrl+C teardown (flipping a cancel flag the drive loop polls), so
/// letting the close-all chord fire would drop the overlay without
/// aborting the login task.
fn overlay_open_no_login(cx: &HostCtx) -> bool {
    overlay_open(cx) && !cx.login_active
}

/// Translate a parsed chord into a vaxis activator.
///
/// Panics on a named key the vaxis name table doesn't know. The chords
/// come from the compiled default table (and later from validated user
/// config), so an unresolvable name is a bug in the data layer, not a
/// runtime condition.
fn activator(spec: &ChordSpec) -> Activator {
    let codepoint = match spec.key {
        ChordKey::Char(c) => u32::from(c),
        ChordKey::Named(name) => {
            name_map(name).unwrap_or_else(|| panic!("vaxis knows the key name {name:?}"))
        }
        ChordKey::F(n) => {
            name_map(&format!("f{n}")).unwrap_or_else(|| panic!("vaxis knows the key name f{n}"))
        }
    };
    let mut mods = Modifiers::empty();
    if spec.ctrl {
        mods |= Modifiers::CTRL;
    }
    if spec.alt {
        mods |= Modifiers::ALT;
    }
    if spec.shift {
        mods |= Modifiers::SHIFT;
    }
    if spec.super_mod {
        mods |= Modifiers::SUPER;
    }
    Activator::new(codepoint, mods)
}

/// Whether `key` activates `action_id`'s effective chord.
///
/// Overlay-local chords (Spec F) are matched at-target rather than through the
/// global keymap, but they must still read the same source of truth as their
/// hint labels. Resolving the action's effective chord here (the same data
/// [`aj_app::keybindings::action_shortcut`] renders) keeps match and label from
/// drifting, and picks up a user `[keybindings]` override the same way.
///
/// Panics if `action_id` has no chord, since these callers pass compile-time
/// action constants whose chords the data layer guarantees.
pub(crate) fn action_matches(key: &Key, action_id: &str) -> bool {
    let chord = aj_app::keybindings::effective_chord(action_id)
        .and_then(aj_app::actions::parse_chord)
        .unwrap_or_else(|| panic!("aj knows the effective chord for {action_id}"));
    activator(&chord).accepts(key)
}

/// Build the global keymap: the fixed Ctrl+C ladder plus the compiled
/// default bindings, each gated by the predicate `aj` expresses as host
/// conditionals.
pub(crate) fn build_keymap() -> Keymap<AjAction, HostCtx> {
    // The Ctrl+C ladder, a fixed terminal convention rather than a
    // rebindable binding (see `aj_app::keybindings::fixed_keys`). The
    // rungs are selected purely by predicate:
    //
    // - Overlay open: both rungs decline, `CloseAllOverlays` (bound to
    //   ctrl+c in the default table) fires instead.
    // - Turn running: the Cancel single fires and the quit sequence
    //   never arms.
    // - Idle: the first ctrl+c arms the quit sequence, the second
    //   completes it. Gating the sequence on `!turn_running` matters
    //   beyond arming: the engine re-checks predicates on every
    //   advance, so an armed quit drops out when a turn starts and the
    //   next ctrl+c falls through to Cancel instead of quitting.
    let ctrl_c = Activator::new(u32::from('c'), Modifiers::CTRL);
    let mut entries = vec![
        Entry::single(ctrl_c, AjAction::CancelTurn, BindingPhase::Capture).with_enabled(can_cancel),
        Entry::sequence(vec![ctrl_c, ctrl_c], AjAction::Quit).with_enabled(can_arm_quit),
    ];

    for binding in global_bindings() {
        let phase = match binding.phase {
            ChordPhase::Capture => BindingPhase::Capture,
            ChordPhase::Bubble => BindingPhase::Bubble,
        };
        // The predicates mirror aj's host conditionals: the overlay
        // openers and the queue gestures are inert while a modal is up,
        // close-all only exists while one is, and the render toggles
        // plus the clipboard paste work regardless. Chat page-scroll is
        // inert under a modal too, because an open overlay owns its own
        // PageUp/PageDown (Spec E section 1 routes the page keys to the
        // chat only when nothing is capturing).
        //
        // NOTE(aljoscha): the `global_bindings` phase puts the page
        // and Home/End chords in the capture phase, ahead of the focused
        // editor, whose `TextArea` otherwise consumes PageUp/PageDown for its
        // own multi-line scroll and Home/End for line start/end. That editor
        // scroll is deliberately superseded by chat scroll: the editor is a
        // compose box that auto-scrolls to the cursor, its line-start/line-end
        // motion stays on the Emacs-style Ctrl+A/Ctrl+E, and the arrow keys
        // still move within it.
        //
        // NOTE(aljoscha): Spec E.1 also lists half-page (Ctrl+U/Ctrl+D) as
        // chat-scroll keys, but those are editor chords in the Emacs-style
        // `TextArea` (kill-to-start, delete-forward), so they can't double as
        // editor-focused chat-scroll chords. Half-page scroll is not bound yet.
        let enabled: fn(&HostCtx) -> bool = match binding.action {
            AjAction::CloseAllOverlays => overlay_open_no_login,
            AjAction::PaletteOpen
            | AjAction::HistoryOpen
            | AjAction::AgentPickerOpen
            | AjAction::Dequeue
            | AjAction::ChatPageUp
            | AjAction::ChatPageDown
            | AjAction::ChatScrollToTop
            | AjAction::ChatScrollToBottom
            // The sidebar gestures are inert under an overlay for the same
            // reason the rest are: a modal owns the keyboard, and switching the
            // session under an open overlay would leave it scoped to a session
            // nobody is looking at.
            | AjAction::SidebarToggle
            | AjAction::SidebarFold
            | AjAction::SidebarArchived
            | AjAction::SessionNext
            | AjAction::SessionPrev
            | AjAction::SessionNew
            | AjAction::SessionTag
            | AjAction::SessionArchive => no_overlay,
            // Alt+Enter submits editor text, so it is inert when the transcript
            // or an overlay owns focus.
            AjAction::Steer => in_editor_focus,
            // Transcript focus is gated to the autocomplete popup being closed,
            // so Tab focuses the transcript with the popup down and stays the
            // editor's accept key with it up (see `focus_enabled`).
            AjAction::TranscriptFocus => focus_enabled,
            // The copy chord is live only while the transcript is focused (see
            // `in_transcript_focus`), so `y` types normally in the editor. The
            // branch chord (`b`) rides the same gate for the same reason.
            AjAction::CopyMessage | AjAction::BranchMessage => in_transcript_focus,
            _ => |_| true,
        };
        entries.push(
            Entry::single(activator(&binding.chord), binding.action, phase).with_enabled(enabled),
        );
    }

    // Plain Up / Ctrl+P recall a pending message, mirroring `aj`. These are not
    // rebindable bindings but the editor's own cursor-up keys (see `TextArea`),
    // intercepted in the capture phase ahead of it under a stricter gate than
    // the `alt+up` dequeue: only an empty editor with a message pending yanks.
    // When the gate declines, the capture single does not fire, so the key
    // descends to the editor for normal history / cursor nav (a declined
    // capture single consumes nothing, and no sequence starts on these keys).
    // Both fire the same `Dequeue` action the drive loop already handles.
    for recall in [
        Activator::new(Key::UP, Modifiers::empty()),
        Activator::new(u32::from('p'), Modifiers::CTRL),
    ] {
        entries.push(
            Entry::single(recall, AjAction::Dequeue, BindingPhase::Capture)
                .with_enabled(can_recall_pending),
        );
    }
    Keymap::new(entries)
}

#[cfg(test)]
mod tests {
    use aj_agent::message::AgentMessage;
    use aj_app::actions::parse_chord;
    use aj_models::types::{Message, UserMessage};
    use aj_wire::AgentQueue;
    use vaxis::key::Key;

    use super::*;

    fn ctx(turn_running: bool) -> HostCtx {
        HostCtx {
            overlays: Rc::new(RefCell::new(OverlayStack::default())),
            editor: TextArea::new(),
            focus_mode: Rc::new(std::cell::Cell::new(false)),
            turn_running,
            login_active: false,
            chat: Rc::new(RefCell::new(ChatState::new(
                aj_agent::events::AgentSettings {
                    provider: "scripted".into(),
                    model_id: "scripted".into(),
                    thinking: "off".into(),
                    thinking_display: "default".into(),
                    speed: "standard".into(),
                    verbosity: "default".into(),
                },
                0,
                std::sync::Arc::new(Vec::new()),
            ))),
            active_view: AgentId::Main,
        }
    }

    /// Note a queued message for the active view, the way a `QueueUpdate`
    /// frame does.
    fn queue(cx: &HostCtx, steering: &[&str], follow_up: &[&str]) {
        let messages = |texts: &[&str]| {
            texts
                .iter()
                .map(|text| {
                    AgentMessage::wire(Message::User(UserMessage::text((*text).to_string())))
                })
                .collect()
        };
        cx.chat.borrow_mut().note_queue(AgentQueue {
            agent_id: cx.active_view,
            steering: messages(steering),
            follow_up: messages(follow_up),
        });
    }

    /// A context for the recall gesture: an editor holding `editor_text` and,
    /// when `pending`, a queued message for the active view.
    fn recall_ctx(editor_text: &str, pending: bool) -> HostCtx {
        let cx = ctx(false);
        if !editor_text.is_empty() {
            cx.editor.borrow_mut().set_text(editor_text);
        }
        if pending {
            queue(&cx, &[], &["queued"]);
        }
        cx
    }

    /// A recall context in transcript-focus mode with a message pending: the
    /// setup where plain Up / Ctrl+P must step through user messages rather
    /// than recall.
    fn focus_recall_ctx() -> HostCtx {
        let cx = recall_ctx("", true);
        cx.focus_mode.set(true);
        cx
    }

    fn push_scrim(cx: &HostCtx) {
        cx.overlays.borrow_mut().push(crate::overlay::OpenOverlay {
            widget: Rc::new(RefCell::new(crate::overlay::Scrim)),
            focus: Rc::new(RefCell::new(crate::overlay::Scrim)),
            placement: crate::overlay::OverlayPlacement::Small,
        });
    }

    /// A context whose editor holds a draft but has no autocomplete popup open,
    /// so the transcript-focus chord (Tab) still matches: a draft does not block
    /// focusing the transcript.
    fn drafting_ctx() -> HostCtx {
        let cx = ctx(false);
        cx.editor.borrow_mut().set_text("hello");
        cx
    }

    fn key(codepoint: u32, mods: Modifiers) -> Key {
        Key {
            codepoint,
            mods,
            ..Key::default()
        }
    }

    /// Build the `Key` a `ChordSpec` activates, mirroring [`activator`], so a
    /// test can drive a compiled binding's chord through the keymap.
    fn key_of(spec: &ChordSpec) -> Key {
        let codepoint = match spec.key {
            ChordKey::Char(c) => u32::from(c),
            ChordKey::Named(name) => name_map(name).expect("vaxis knows the key name"),
            ChordKey::F(n) => name_map(&format!("f{n}")).expect("vaxis knows the f-key"),
        };
        let mut mods = Modifiers::empty();
        if spec.ctrl {
            mods |= Modifiers::CTRL;
        }
        if spec.alt {
            mods |= Modifiers::ALT;
        }
        if spec.shift {
            mods |= Modifiers::SHIFT;
        }
        if spec.super_mod {
            mods |= Modifiers::SUPER;
        }
        key(codepoint, mods)
    }

    /// The control code a terminal sends for `ctrl+<c>`, in the conventional
    /// caret mapping. `None` where the mapping has no entry, and ctrl then
    /// never reaches the application at all.
    fn control_code(c: char) -> Option<u8> {
        Some(match c {
            // Ctrl+Space and Ctrl+@ are the same keystroke to a terminal.
            '@' | ' ' => 0x00,
            'a'..='z' => u8::try_from(u32::from(c) - 0x60).expect("an ascii letter"),
            '[' => 0x1b,
            '\\' => 0x1c,
            ']' => 0x1d,
            '^' => 0x1e,
            '_' => 0x1f,
            '?' => 0x7f,
            _ => return None,
        })
    }

    /// The control code a terminal sends for a named key that has one, rather
    /// than an escape sequence.
    fn control_code_key(name: &str) -> Option<u8> {
        match name {
            "enter" => Some(0x0d),
            "escape" => Some(0x1b),
            "tab" => Some(0x09),
            "backspace" => Some(0x7f),
            _ => None,
        }
    }

    /// The modifier parameter a CSI sequence carries: one plus the bitmask of
    /// shift, alt, ctrl, and super in that bit order. `None` when no modifier is
    /// held, where a terminal leaves the parameter out entirely.
    fn modifier_param(spec: &ChordSpec) -> Option<u8> {
        let mask = u8::from(spec.shift)
            | u8::from(spec.alt) << 1
            | u8::from(spec.ctrl) << 2
            | u8::from(spec.super_mod) << 3;
        (mask != 0).then_some(mask + 1)
    }

    /// The `CSI 1 ; <mods> <final>` form of a cursor key.
    fn csi_final(final_byte: u8, spec: &ChordSpec) -> Vec<u8> {
        let final_byte = char::from(final_byte);
        match modifier_param(spec) {
            Some(param) => format!("\x1b[1;{param}{final_byte}").into_bytes(),
            None => format!("\x1b[{final_byte}").into_bytes(),
        }
    }

    /// The `CSI <number> ; <mods> ~` form of an editing or function key.
    fn csi_numbered(number: u8, spec: &ChordSpec) -> Vec<u8> {
        match modifier_param(spec) {
            Some(param) => format!("\x1b[{number};{param}~").into_bytes(),
            None => format!("\x1b[{number}~").into_bytes(),
        }
    }

    fn char_bytes(c: char, spec: &ChordSpec) -> Option<Vec<u8>> {
        // A bare control byte is how a ctrl chord or a named key is
        // transmitted, no key sends one on its own.
        if c.is_control() {
            return None;
        }
        let mut buf = [0u8; 4];
        let utf8 = c.encode_utf8(&mut buf).as_bytes();
        // (ctrl, alt, shift, super)
        match (spec.ctrl, spec.alt, spec.shift, spec.super_mod) {
            (false, false, false, false) => Some(utf8.to_vec()),
            // Shift arrives as the shifted glyph, which we can only synthesize
            // for a letter. Every other one depends on the keyboard layout.
            (false, false, true, false) => c
                .is_ascii_alphabetic()
                .then(|| vec![u8::try_from(c.to_ascii_uppercase()).expect("an ascii letter")]),
            (true, false, false, false) => control_code(c).map(|code| vec![code]),
            (false, true, false, false) => {
                let mut bytes = vec![0x1b];
                bytes.extend_from_slice(utf8);
                Some(bytes)
            }
            // A character key is its own bytes with at most an ESC prefix for
            // alt, so there is nowhere to put a second modifier.
            _ => None,
        }
    }

    fn named_bytes(name: &str, spec: &ChordSpec) -> Option<Vec<u8>> {
        if let Some(code) = control_code_key(name) {
            // (ctrl, alt, shift, super)
            return match (spec.ctrl, spec.alt, spec.shift, spec.super_mod) {
                (false, false, false, false) => Some(vec![code]),
                (false, true, false, false) => Some(vec![0x1b, code]),
                // `CSI Z` is the only modified control-code key terminals send.
                (false, false, true, false) if name == "tab" => Some(b"\x1b[Z".to_vec()),
                _ => None,
            };
        }
        Some(match name {
            "up" => csi_final(b'A', spec),
            "down" => csi_final(b'B', spec),
            "right" => csi_final(b'C', spec),
            "left" => csi_final(b'D', spec),
            "end" => csi_final(b'F', spec),
            "home" => csi_final(b'H', spec),
            "insert" => csi_numbered(2, spec),
            "delete" => csi_numbered(3, spec),
            "page_up" => csi_numbered(5, spec),
            "page_down" => csi_numbered(6, spec),
            _ => panic!("the chord grammar's key names are a closed set, got {name:?}"),
        })
    }

    /// The bytes a terminal sends when the user types `spec`, or `None` when no
    /// keystroke sends it at all.
    ///
    /// This is the encoding every terminal speaks. One that has negotiated the
    /// kitty keyboard protocol sends `CSI <codepoint> ; <mods> u` for everything
    /// instead, which would carry far more chords, but the protocol is
    /// opportunistic and `aj` cannot require it.
    fn terminal_bytes(spec: &ChordSpec) -> Option<Vec<u8>> {
        match spec.key {
            ChordKey::Char(c) => char_bytes(c, spec),
            ChordKey::Named(name) => named_bytes(name, spec),
            // The numbered CSI forms, xterm's numbering, whose gaps are the
            // numbers it spends on other keys. It runs out at F20.
            ChordKey::F(n) => [
                11, 12, 13, 14, 15, 17, 18, 19, 20, 21, 23, 24, 25, 26, 28, 29, 31, 32, 33, 34,
            ]
            .get(usize::from(n).checked_sub(1)?)
            .map(|number| csi_numbered(*number, spec)),
        }
    }

    /// The key press a terminal's bytes for `spec` decode to, or `None` when no
    /// keystroke produces one.
    fn terminal_key(spec: &ChordSpec) -> Option<Key> {
        let bytes = terminal_bytes(spec)?;
        let parsed = vaxis::parser::Parser::new().parse(&bytes).ok()?;
        // A short read means the parser took the bytes for the prefix of a
        // longer sequence, which is the `alt+[` hazard exactly: a terminal
        // cannot tell them from the start of one either.
        if parsed.n != bytes.len() {
            return None;
        }
        match parsed.event {
            Some(vaxis::event::Event::KeyPress(key)) => Some(key),
            _ => None,
        }
    }

    /// The named keys [`parse_chord`] emits.
    const CHORD_KEY_NAMES: &[&str] = &[
        "enter",
        "escape",
        "tab",
        "backspace",
        "delete",
        "insert",
        "up",
        "down",
        "left",
        "right",
        "home",
        "end",
        "page_up",
        "page_down",
    ];

    /// Every chord shape the grammar can build: each key class crossed with all
    /// sixteen modifier combinations. The characters span printable ASCII plus a
    /// control character and two multi-byte ones, which a config file can spell
    /// even though no keyboard has such a key.
    fn every_chord_spec() -> impl Iterator<Item = ChordSpec> {
        let chars = (0x20u8..=0x7e)
            .map(|b| ChordKey::Char(char::from(b)))
            .chain(['\t', 'ä', '€'].into_iter().map(ChordKey::Char));
        let named = CHORD_KEY_NAMES.iter().copied().map(ChordKey::Named);
        let fkeys = (1u8..=35).map(ChordKey::F);
        chars.chain(named).chain(fkeys).flat_map(|key| {
            (0u8..16).map(move |bits| ChordSpec {
                key,
                ctrl: bits & 1 != 0,
                alt: bits & 2 != 0,
                shift: bits & 4 != 0,
                super_mod: bits & 8 != 0,
            })
        })
    }

    /// A context in which `action`'s predicate holds, so a keymap match tests
    /// the chord rather than the gate.
    fn ctx_where_enabled(action: AjAction) -> HostCtx {
        let cx = ctx(false);
        match action {
            AjAction::CloseAllOverlays => push_scrim(&cx),
            AjAction::CopyMessage | AjAction::BranchMessage => cx.focus_mode.set(true),
            _ => {}
        }
        cx
    }

    #[test]
    fn activator_translates_chars_named_keys_and_fkeys() {
        let alt_up = activator(&parse_chord("alt+up").unwrap());
        assert_eq!(alt_up, Activator::new(Key::UP, Modifiers::ALT));

        let alt_enter = activator(&parse_chord("alt+enter").unwrap());
        assert_eq!(alt_enter, Activator::new(Key::ENTER, Modifiers::ALT));

        let ctrl_plus = activator(&parse_chord("ctrl++").unwrap());
        assert_eq!(ctrl_plus, Activator::new(u32::from('+'), Modifiers::CTRL));

        let f5 = activator(&parse_chord("shift+f5").unwrap());
        assert_eq!(f5, Activator::new(Key::F5, Modifiers::SHIFT));

        let super_k = activator(&parse_chord("super+k").unwrap());
        assert_eq!(super_k, Activator::new(u32::from('k'), Modifiers::SUPER));
    }

    /// Drift guard: `aj_app::actions::ALWAYS_ON_ACTION_IDS` must match the
    /// keymap's predicates. An always-on global is one that fires both under a
    /// modal overlay and idle, i.e. its predicate ignores overlay and focus
    /// state. `install_keybindings` checks overlay-local overrides against that
    /// declared set, so if the two disagree the shadow check guards the wrong
    /// chords. Computing the set from the compiled keymap keeps aj-app's
    /// declaration honest.
    #[test]
    fn always_on_globals_match_aj_app_declaration() {
        use std::collections::BTreeSet;

        let keymap = build_keymap();
        let idle = ctx(false);
        let modal = ctx(false);
        push_scrim(&modal);

        let mut fires_regardless: BTreeSet<&str> = BTreeSet::new();
        for binding in global_bindings() {
            let k = key_of(&binding.chord);
            let idle_hit =
                keymap.match_single(&k, BindingPhase::Capture, &idle) == Some(&binding.action);
            let modal_hit =
                keymap.match_single(&k, BindingPhase::Capture, &modal) == Some(&binding.action);
            if idle_hit && modal_hit {
                fires_regardless.insert(binding.action.action_id().expect("global has an id"));
            }
        }

        let declared: BTreeSet<&str> = aj_app::actions::ALWAYS_ON_ACTION_IDS
            .iter()
            .copied()
            .collect();
        assert_eq!(
            fires_regardless, declared,
            "the always-on set drifted from the keymap predicates",
        );
    }

    /// Every default chord survives a round trip through the terminal: the bytes
    /// a terminal sends for it parse back into the key its binding matches on,
    /// and that key reaches this binding's own action and no other.
    ///
    /// This is not a formality. A terminal has far fewer encodings than the
    /// chord grammar has spellings, so a well-formed chord can still be
    /// unreachable, and the failure is silent. Alt is sent as an ESC prefix, so
    /// `alt+[` arrives as the CSI introducer every escape sequence starts with.
    /// Ctrl+i is sent as the tab control code, so it fires the Tab binding
    /// instead. Nothing else in the suite notices, because the keymap is
    /// otherwise only ever fed a `Key` reconstructed from the chord rather than
    /// one the parser produced.
    #[test]
    fn every_default_chord_survives_the_terminal() {
        let keymap = build_keymap();
        let globals = global_bindings();
        for (action_id, chord, _) in aj_app::keybindings::AJ_KEYBINDINGS {
            let spec = parse_chord(chord).expect("the default chords parse");
            let key = terminal_key(&spec).unwrap_or_else(|| {
                panic!(
                    "{action_id}'s chord {chord:?} is not typeable: the bytes a terminal \
                     sends for it are not a key press",
                )
            });
            assert!(
                activator(&spec).accepts(&key),
                "{action_id}'s chord {chord:?} arrives from the terminal as a different \
                 key: {key:?}",
            );

            match globals
                .iter()
                .find(|b| b.action.action_id() == Some(*action_id))
            {
                Some(binding) => assert_eq!(
                    keymap.match_single(
                        &key,
                        BindingPhase::Capture,
                        &ctx_where_enabled(binding.action)
                    ),
                    Some(&binding.action),
                    "{action_id} does not fire for the key its own chord produces",
                ),
                // Overlay-local rows never enter the global keymap. The focused
                // widget matches them at target through `action_matches`.
                None => assert!(
                    action_matches(&key, action_id),
                    "{action_id} does not match the key its own chord produces",
                ),
            }
        }
    }

    /// [`aj_app::actions::untypeable_reason`] is a hand-written restatement of
    /// what the input parser does, since `aj-app` may not depend on this crate.
    /// This sweeps the whole chord space through the real parser and asserts the
    /// two agree, so the restatement cannot drift from the parser it describes.
    #[test]
    fn untypeable_reason_agrees_with_the_parser() {
        for spec in every_chord_spec() {
            let round_trips = terminal_key(&spec).is_some_and(|key| activator(&spec).accepts(&key));
            assert_eq!(
                round_trips,
                aj_app::actions::untypeable_reason(&spec).is_none(),
                "the predicate and the parser disagree about {spec:?}",
            );
        }
    }

    /// The compiled keymap resolves the ctrl+c ambiguity by predicate:
    /// close-all under an overlay, cancel while a turn runs, the quit
    /// sequence otherwise.
    #[test]
    fn ctrl_c_routes_by_context() {
        let keymap = build_keymap();
        let ctrl_c = key(u32::from('c'), Modifiers::CTRL);

        let idle = ctx(false);
        assert_eq!(
            keymap.match_single(&ctrl_c, BindingPhase::Capture, &idle),
            None,
            "idle: no single fires, the quit sequence arms"
        );
        assert!(keymap.starts_sequence(&ctrl_c, &idle));

        let running = ctx(true);
        assert_eq!(
            keymap.match_single(&ctrl_c, BindingPhase::Capture, &running),
            Some(&AjAction::CancelTurn)
        );
        assert!(
            !keymap.starts_sequence(&ctrl_c, &running),
            "the quit sequence is gated off while a turn runs"
        );

        let modal = ctx(true);
        modal
            .overlays
            .borrow_mut()
            .push(crate::overlay::OpenOverlay {
                widget: Rc::new(RefCell::new(crate::overlay::Scrim)),
                focus: Rc::new(RefCell::new(crate::overlay::Scrim)),
                placement: crate::overlay::OverlayPlacement::Small,
            });
        assert_eq!(
            keymap.match_single(&ctrl_c, BindingPhase::Capture, &modal),
            Some(&AjAction::CloseAllOverlays),
            "an open overlay wins even over a running turn"
        );
        assert!(!keymap.starts_sequence(&ctrl_c, &modal));
    }

    /// The overlay openers and queue gestures are inert under a modal,
    /// while the render toggles keep working.
    #[test]
    fn overlay_gating_of_the_global_chords() {
        let keymap = build_keymap();
        let modal = ctx(false);
        modal
            .overlays
            .borrow_mut()
            .push(crate::overlay::OpenOverlay {
                widget: Rc::new(RefCell::new(crate::overlay::Scrim)),
                focus: Rc::new(RefCell::new(crate::overlay::Scrim)),
                placement: crate::overlay::OverlayPlacement::Small,
            });

        let ctrl_o = key(u32::from('o'), Modifiers::CTRL);
        let alt_o = key(u32::from('o'), Modifiers::ALT);
        let alt_enter = key(Key::ENTER, Modifiers::ALT);

        assert_eq!(
            keymap.match_single(&ctrl_o, BindingPhase::Capture, &modal),
            None,
            "palette-open is inert under a modal"
        );
        assert_eq!(
            keymap.match_single(&alt_enter, BindingPhase::Capture, &modal),
            None,
            "steer is inert under a modal"
        );
        assert_eq!(
            keymap.match_single(&alt_o, BindingPhase::Capture, &modal),
            Some(&AjAction::ToolsExpand),
            "the render toggles work under a modal, matching aj"
        );

        let idle = ctx(false);
        assert_eq!(
            keymap.match_single(&ctrl_o, BindingPhase::Capture, &idle),
            Some(&AjAction::PaletteOpen)
        );
        assert_eq!(
            keymap.match_single(&alt_enter, BindingPhase::Capture, &idle),
            Some(&AjAction::Steer)
        );

        let mut focused = ctx(false);
        focused.focus_mode.set(true);
        assert_eq!(
            keymap.match_single(&alt_enter, BindingPhase::Capture, &focused),
            None,
            "steer is inert while the transcript owns focus"
        );
        focused.turn_running = true;
        assert_eq!(
            keymap.match_single(&alt_enter, BindingPhase::Capture, &focused),
            None,
            "busy steering is also inert outside the editor"
        );
    }

    /// The chat page-scroll chords match in the capture phase (before the
    /// editor's own PageUp/PageDown scroll) and go inert while a modal is up,
    /// where the overlay owns its own page keys.
    #[test]
    fn chat_page_scroll_matches_in_capture_and_is_gated_by_overlays() {
        let keymap = build_keymap();
        let page_up = key(Key::PAGE_UP, Modifiers::empty());
        let page_down = key(Key::PAGE_DOWN, Modifiers::empty());

        let idle = ctx(false);
        assert_eq!(
            keymap.match_single(&page_up, BindingPhase::Capture, &idle),
            Some(&AjAction::ChatPageUp),
        );
        assert_eq!(
            keymap.match_single(&page_down, BindingPhase::Capture, &idle),
            Some(&AjAction::ChatPageDown),
        );

        let modal = ctx(false);
        modal
            .overlays
            .borrow_mut()
            .push(crate::overlay::OpenOverlay {
                widget: Rc::new(RefCell::new(crate::overlay::Scrim)),
                focus: Rc::new(RefCell::new(crate::overlay::Scrim)),
                placement: crate::overlay::OverlayPlacement::Small,
            });
        assert_eq!(
            keymap.match_single(&page_up, BindingPhase::Capture, &modal),
            None,
            "an open overlay owns its own PageUp",
        );
        assert_eq!(
            keymap.match_single(&page_down, BindingPhase::Capture, &modal),
            None,
            "an open overlay owns its own PageDown",
        );
    }

    /// The Home/End chat-scroll chords match in the capture phase (before
    /// the editor's own line-start/line-end motion, which stays on
    /// Ctrl+A/Ctrl+E) and go inert while a modal is up, where the overlay
    /// owns its own Home/End.
    #[test]
    fn chat_scroll_home_end_matches_in_capture_and_is_gated_by_overlays() {
        let keymap = build_keymap();
        let home = key(Key::HOME, Modifiers::empty());
        let end = key(Key::END, Modifiers::empty());

        let idle = ctx(false);
        assert_eq!(
            keymap.match_single(&home, BindingPhase::Capture, &idle),
            Some(&AjAction::ChatScrollToTop),
        );
        assert_eq!(
            keymap.match_single(&end, BindingPhase::Capture, &idle),
            Some(&AjAction::ChatScrollToBottom),
        );

        let modal = ctx(false);
        modal
            .overlays
            .borrow_mut()
            .push(crate::overlay::OpenOverlay {
                widget: Rc::new(RefCell::new(crate::overlay::Scrim)),
                focus: Rc::new(RefCell::new(crate::overlay::Scrim)),
                placement: crate::overlay::OverlayPlacement::Small,
            });
        assert_eq!(
            keymap.match_single(&home, BindingPhase::Capture, &modal),
            None,
            "an open overlay owns its own Home",
        );
        assert_eq!(
            keymap.match_single(&end, BindingPhase::Capture, &modal),
            None,
            "an open overlay owns its own End",
        );
    }

    /// Transcript-focus resolves in the capture phase (ahead of the editor)
    /// whenever the autocomplete popup is closed, including with a draft in the
    /// editor, and is inert while a capturing overlay is up. Only an open popup
    /// keeps Tab for the editor (to apply a completion).
    #[test]
    fn transcript_focus_matches_on_tab_when_the_autocomplete_popup_is_closed() {
        let keymap = build_keymap();
        let tab = key(Key::TAB, Modifiers::empty());

        let idle = ctx(false);
        assert_eq!(
            keymap.match_single(&tab, BindingPhase::Capture, &idle),
            Some(&AjAction::TranscriptFocus),
            "Tab enters transcript-focus in the capture phase with an empty editor",
        );

        let drafting = drafting_ctx();
        assert_eq!(
            keymap.match_single(&tab, BindingPhase::Capture, &drafting),
            Some(&AjAction::TranscriptFocus),
            "a draft with no popup still enters transcript-focus",
        );

        let modal = ctx(false);
        modal
            .overlays
            .borrow_mut()
            .push(crate::overlay::OpenOverlay {
                widget: Rc::new(RefCell::new(crate::overlay::Scrim)),
                focus: Rc::new(RefCell::new(crate::overlay::Scrim)),
                placement: crate::overlay::OverlayPlacement::Small,
            });
        assert_eq!(
            keymap.match_single(&tab, BindingPhase::Capture, &modal),
            None,
            "transcript-focus is inert while an overlay is open",
        );
    }

    /// The copy chord (`y`) matches in the capture phase only while the
    /// transcript-focus flag is set. With it clear (the editor focused) it
    /// declines, so `y` descends to the editor and types normally.
    #[test]
    fn copy_message_matches_on_y_only_in_transcript_focus() {
        let keymap = build_keymap();
        let y = key(u32::from('y'), Modifiers::empty());

        let editor_focused = ctx(false);
        assert_eq!(
            keymap.match_single(&y, BindingPhase::Capture, &editor_focused),
            None,
            "not in focus mode: y is not captured and types in the editor",
        );

        let transcript_focused = ctx(false);
        transcript_focused.focus_mode.set(true);
        assert_eq!(
            keymap.match_single(&y, BindingPhase::Capture, &transcript_focused),
            Some(&AjAction::CopyMessage),
            "focus mode: y copies the focused message",
        );
    }

    /// The branch chord (`b`) matches in the capture phase only while the
    /// transcript-focus flag is set, mirroring the copy chord: with the editor
    /// focused it declines, so `b` descends to the editor and types normally.
    #[test]
    fn branch_message_matches_on_b_only_in_transcript_focus() {
        let keymap = build_keymap();
        let b = key(u32::from('b'), Modifiers::empty());

        let editor_focused = ctx(false);
        assert_eq!(
            keymap.match_single(&b, BindingPhase::Capture, &editor_focused),
            None,
            "not in focus mode: b is not captured and types in the editor",
        );

        let transcript_focused = ctx(false);
        transcript_focused.focus_mode.set(true);
        assert_eq!(
            keymap.match_single(&b, BindingPhase::Capture, &transcript_focused),
            Some(&AjAction::BranchMessage),
            "focus mode: b branches from the focused message",
        );
    }

    /// `action_matches` resolves an action's chord from the shared keybinding
    /// data, so the overlay-local matchers read the same source of truth as
    /// their hint labels. `ACTION_TASK_KILL` defaults to ctrl+k: the matching
    /// key activates it and a different key does not.
    #[test]
    fn action_matches_resolves_the_default_chord() {
        use aj_app::keybindings::ACTION_TASK_KILL;

        let ctrl_k = key(u32::from('k'), Modifiers::CTRL);
        assert!(
            action_matches(&ctrl_k, ACTION_TASK_KILL),
            "ctrl+k is the default chord for the task-kill action",
        );

        let ctrl_j = key(u32::from('j'), Modifiers::CTRL);
        assert!(
            !action_matches(&ctrl_j, ACTION_TASK_KILL),
            "a different key does not match the resolved chord",
        );
        let plain_k = key(u32::from('k'), Modifiers::empty());
        assert!(
            !action_matches(&plain_k, ACTION_TASK_KILL),
            "the modifiers must match too",
        );
    }

    /// Plain Up / Ctrl+P recall a pending message only when the editor is empty
    /// and something is pending, mirroring `aj`'s gating. Otherwise the capture
    /// single declines and the key falls through to the editor.
    #[test]
    fn up_and_ctrl_p_recall_pending_only_when_editor_empty_and_pending() {
        let keymap = build_keymap();
        let up = key(Key::UP, Modifiers::empty());
        let ctrl_p = key(u32::from('p'), Modifiers::CTRL);

        // Empty editor + pending: both keys fire Dequeue in the capture phase.
        let ready = recall_ctx("", true);
        assert_eq!(
            keymap.match_single(&up, BindingPhase::Capture, &ready),
            Some(&AjAction::Dequeue),
            "Up recalls with an empty editor and a pending message",
        );
        assert_eq!(
            keymap.match_single(&ctrl_p, BindingPhase::Capture, &ready),
            Some(&AjAction::Dequeue),
            "Ctrl+P recalls the same way",
        );

        // A draft in the editor: the recall declines and, crucially, neither a
        // capture single nor a sequence consumes the key, so it descends to the
        // editor for normal history / cursor nav.
        let drafting = recall_ctx("draft", true);
        assert_eq!(
            keymap.match_single(&up, BindingPhase::Capture, &drafting),
            None,
            "a draft in the editor falls through to the editor",
        );
        assert!(
            !keymap.starts_sequence(&up, &drafting),
            "no sequence starts on Up, so a declined recall does not swallow it",
        );

        // Nothing pending: the recall declines even with an empty editor.
        let idle = recall_ctx("", false);
        assert_eq!(
            keymap.match_single(&up, BindingPhase::Capture, &idle),
            None,
            "no pending message: Up is normal history nav",
        );
        assert_eq!(
            keymap.match_single(&ctrl_p, BindingPhase::Capture, &idle),
            None,
        );

        // An open overlay gates the recall off, like the other queue gestures.
        let modal = recall_ctx("", true);
        push_scrim(&modal);
        assert_eq!(
            keymap.match_single(&up, BindingPhase::Capture, &modal),
            None,
            "an open overlay owns its own Up",
        );
    }

    /// In transcript focus, plain Up / Ctrl+P step through the user messages
    /// (bubble-phase `handle_focus_key`), not recall. The capture-phase recall
    /// single must therefore decline while focused, so the key descends to the
    /// transcript's stepping. Without the focus gate the capture recall would
    /// pre-empt the bubble-phase stepping.
    #[test]
    fn recall_declines_in_transcript_focus() {
        let keymap = build_keymap();
        let up = key(Key::UP, Modifiers::empty());
        let ctrl_p = key(u32::from('p'), Modifiers::CTRL);

        let focused = focus_recall_ctx();
        assert_eq!(
            keymap.match_single(&up, BindingPhase::Capture, &focused),
            None,
            "Up steps messages in focus mode, it does not recall",
        );
        assert_eq!(
            keymap.match_single(&ctrl_p, BindingPhase::Capture, &focused),
            None,
            "Ctrl+P steps messages in focus mode too",
        );
    }

    /// A queued steering message (the Alt+Enter-with-text path) is recalled by
    /// Up just like a follow-up: either vector counts as pending, so the recall
    /// gate fires for either kind. This pins the "queued/steering" case.
    #[test]
    fn up_recalls_a_queued_steering_message() {
        let keymap = build_keymap();
        let up = key(Key::UP, Modifiers::empty());

        let cx = ctx(false);
        queue(&cx, &["steered"], &[]);
        assert_eq!(
            keymap.match_single(&up, BindingPhase::Capture, &cx),
            Some(&AjAction::Dequeue),
            "Up recalls a queued steering message",
        );
    }

    /// A drained agent keeps an empty queue entry in the model, which must not
    /// read as pending: the recall would find nothing to yank and would have
    /// swallowed the key that history navigation wanted.
    #[test]
    fn a_drained_queue_does_not_recall() {
        let keymap = build_keymap();
        let up = key(Key::UP, Modifiers::empty());

        let cx = recall_ctx("", true);
        queue(&cx, &[], &[]);
        assert_eq!(
            keymap.match_single(&up, BindingPhase::Capture, &cx),
            None,
            "an empty queue snapshot is not a pending message",
        );
    }

    /// The alt+up dequeue fires regardless of editor contents or pending state:
    /// its only gate is that no overlay is open. It must not inherit the
    /// stricter recall gate.
    #[test]
    fn alt_up_dequeue_ignores_editor_and_pending() {
        let keymap = build_keymap();
        let alt_up = key(Key::UP, Modifiers::ALT);

        let empty = recall_ctx("", false);
        assert_eq!(
            keymap.match_single(&alt_up, BindingPhase::Capture, &empty),
            Some(&AjAction::Dequeue),
            "alt+up fires even with an empty editor and nothing pending",
        );

        let drafting = recall_ctx("draft", true);
        assert_eq!(
            keymap.match_single(&alt_up, BindingPhase::Capture, &drafting),
            Some(&AjAction::Dequeue),
            "alt+up fires with a draft in the editor too",
        );
    }

    /// While a login dialog is up, the close-all chord (ctrl+c) is inert:
    /// the dialog owns its own Esc/Ctrl+C teardown, so the chord must not
    /// pre-empt it. The dialog is a leaf on the focus path and handles the
    /// key itself.
    #[test]
    fn close_all_is_inert_while_login_is_active() {
        let keymap = build_keymap();
        let ctrl_c = key(u32::from('c'), Modifiers::CTRL);
        let mut modal = ctx(false);
        modal.login_active = true;
        modal
            .overlays
            .borrow_mut()
            .push(crate::overlay::OpenOverlay {
                widget: Rc::new(RefCell::new(crate::overlay::Scrim)),
                focus: Rc::new(RefCell::new(crate::overlay::Scrim)),
                placement: crate::overlay::OverlayPlacement::Small,
            });
        assert_eq!(
            keymap.match_single(&ctrl_c, BindingPhase::Capture, &modal),
            None,
            "ctrl+c falls through to the login dialog, not close-all"
        );
    }
}
