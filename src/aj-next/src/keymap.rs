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

use aj_app::actions::{AjAction, ChordKey, ChordPhase, ChordSpec, default_global_bindings};
use vaxis::key::{Modifiers, name_map};
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
    /// Whether the viewed agent is busy (a binary-driven turn or a
    /// running initial sub-agent spawn), i.e. whether Ctrl+C has
    /// something to cancel.
    pub(crate) turn_running: bool,
    /// Whether an OAuth login dialog is up. It is a modal like the other
    /// overlays, but its own Esc/Ctrl+C handling flips a cancel flag the
    /// drive loop polls, so the close-all chord must not pre-empt it.
    /// The drive loop is this field's single writer.
    pub(crate) login_active: bool,
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

fn can_cancel(cx: &HostCtx) -> bool {
    no_overlay(cx) && cx.turn_running
}

fn can_arm_quit(cx: &HostCtx) -> bool {
    no_overlay(cx) && !cx.turn_running
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

    for binding in default_global_bindings() {
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
        // NOTE(aljoscha): the `default_global_bindings` phase puts the page
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
            | AjAction::Steer
            | AjAction::Dequeue
            | AjAction::ChatPageUp
            | AjAction::ChatPageDown
            | AjAction::ChatScrollToTop
            | AjAction::ChatScrollToBottom => no_overlay,
            // Transcript focus is gated to the autocomplete popup being closed,
            // so Tab focuses the transcript with the popup down and stays the
            // editor's accept key with it up (see `focus_enabled`).
            AjAction::TranscriptFocus => focus_enabled,
            _ => |_| true,
        };
        entries.push(
            Entry::single(activator(&binding.chord), binding.action, phase).with_enabled(enabled),
        );
    }
    Keymap::new(entries)
}

#[cfg(test)]
mod tests {
    use aj_app::actions::parse_chord;
    use vaxis::key::Key;

    use super::*;

    fn ctx(turn_running: bool) -> HostCtx {
        HostCtx {
            overlays: Rc::new(RefCell::new(OverlayStack::default())),
            editor: TextArea::new(),
            turn_running,
            login_active: false,
        }
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
