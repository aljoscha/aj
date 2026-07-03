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
use vaxis::vxfw::{Activator, BindingPhase, Entry, Keymap};

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
    /// Whether the viewed agent is busy (a binary-driven turn or a
    /// running initial sub-agent spawn), i.e. whether Ctrl+C has
    /// something to cancel.
    pub(crate) turn_running: bool,
}

fn overlay_open(cx: &HostCtx) -> bool {
    cx.overlays.borrow().is_open()
}

fn no_overlay(cx: &HostCtx) -> bool {
    !overlay_open(cx)
}

fn can_cancel(cx: &HostCtx) -> bool {
    no_overlay(cx) && cx.turn_running
}

fn can_arm_quit(cx: &HostCtx) -> bool {
    no_overlay(cx) && !cx.turn_running
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
        // plus the clipboard paste work regardless.
        let enabled: fn(&HostCtx) -> bool = match binding.action {
            AjAction::CloseAllOverlays => overlay_open,
            AjAction::PaletteOpen
            | AjAction::HistoryOpen
            | AjAction::AgentPickerOpen
            | AjAction::Steer
            | AjAction::Dequeue => no_overlay,
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
            turn_running,
        }
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
}
