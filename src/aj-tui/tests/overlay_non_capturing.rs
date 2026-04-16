//! Tests for non-capturing overlays.
//!
//! Non-capturing overlays are shown as content but don't steal focus on
//! creation. An application explicitly transfers focus to one via
//! [`Tui::focus_overlay`] and restores the prior focus via
//! [`Tui::unfocus_overlay`]. Input routing also respects the
//! `focused_overlay_id` trumping normal stack-order overlay routing.

mod support;

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::impl_component_any;
use aj_tui::keys::{InputEvent, Key};
use aj_tui::tui::{OverlayAnchor, OverlayOptions, SizeValue, Tui};

use support::{StaticLines, VirtualTerminal, wait_for_render};

// ---------------------------------------------------------------------------
// A focus-aware recorder so tests can assert who received input.
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FocusableRecorder {
    state: Rc<RefCell<FocusState>>,
}

struct FocusState {
    focused: bool,
    inputs: Vec<InputEvent>,
}

impl FocusableRecorder {
    fn new() -> Self {
        Self {
            state: Rc::new(RefCell::new(FocusState {
                focused: false,
                inputs: Vec::new(),
            })),
        }
    }

    fn handle(&self) -> Rc<RefCell<FocusState>> {
        Rc::clone(&self.state)
    }
}

impl Component for FocusableRecorder {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        Vec::new()
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        self.state.borrow_mut().inputs.push(event.clone());
        true
    }

    fn set_focused(&mut self, focused: bool) {
        self.state.borrow_mut().focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.state.borrow().focused
    }
}

fn focused(state: &Rc<RefCell<FocusState>>) -> bool {
    state.borrow().focused
}

fn input_count(state: &Rc<RefCell<FocusState>>) -> usize {
    state.borrow().inputs.len()
}

// ---------------------------------------------------------------------------
// Focus management
// ---------------------------------------------------------------------------

#[test]
fn non_capturing_overlay_preserves_root_focus_on_show() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );
    wait_for_render(&mut tui);

    assert!(focused(&editor_state), "editor should still be focused");
    assert!(
        !focused(&overlay_state),
        "non-capturing overlay should not auto-focus",
    );
}

#[test]
fn focus_overlay_transfers_focus_to_the_overlay() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    tui.focus_overlay(&handle);
    wait_for_render(&mut tui);

    assert!(!focused(&editor_state), "editor should lose focus");
    assert!(focused(&overlay_state), "overlay should receive focus");
    assert!(tui.is_overlay_focused(&handle));
}

#[test]
fn unfocus_overlay_restores_the_previous_focus() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    tui.focus_overlay(&handle);
    tui.unfocus_overlay(&handle);
    wait_for_render(&mut tui);

    assert!(focused(&editor_state), "editor focus should be restored");
    assert!(!focused(&overlay_state));
    assert!(!tui.is_overlay_focused(&handle));
}

// ---------------------------------------------------------------------------
// Input routing
// ---------------------------------------------------------------------------

#[test]
fn non_capturing_overlay_does_not_receive_input_until_focused() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    tui.handle_input(&Key::char('a'));

    assert_eq!(input_count(&editor_state), 1, "editor should get the input");
    assert_eq!(
        input_count(&overlay_state),
        0,
        "non-capturing overlay should not receive input by default",
    );
}

#[test]
fn focused_non_capturing_overlay_receives_input_ahead_of_root() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );
    tui.focus_overlay(&handle);

    tui.handle_input(&Key::char('b'));

    assert_eq!(input_count(&overlay_state), 1);
    assert_eq!(
        input_count(&editor_state),
        0,
        "editor should not see input while overlay owns focus",
    );
}

// ---------------------------------------------------------------------------
// Hidden / shown cycling
// ---------------------------------------------------------------------------

#[test]
fn set_overlay_hidden_drops_the_overlay_out_of_input_and_compositing() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );
    tui.focus_overlay(&handle);

    tui.set_overlay_hidden(&handle, true);
    wait_for_render(&mut tui);

    // Focus restored to editor; overlay no longer receives input.
    assert!(focused(&editor_state));
    assert!(!focused(&overlay_state));
    tui.handle_input(&Key::char('x'));
    assert_eq!(input_count(&editor_state), 1);
    assert_eq!(input_count(&overlay_state), 0);

    // Unhide without re-focusing: overlay still doesn't get input.
    tui.set_overlay_hidden(&handle, false);
    wait_for_render(&mut tui);
    assert!(
        !focused(&overlay_state),
        "unhiding a non-capturing overlay should not auto-focus it",
    );
}

#[test]
fn capturing_overlay_takes_input_without_an_explicit_focus_transfer() {
    // Sanity check: the default routing (topmost non-hidden capturing
    // overlay) still works alongside the focus-transfer path.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    tui.show_overlay(Box::new(overlay), OverlayOptions::default());

    tui.handle_input(&Key::char('c'));

    assert_eq!(input_count(&overlay_state), 1);
    assert_eq!(input_count(&editor_state), 0);
}

// ---------------------------------------------------------------------------
// Rendering still works
// ---------------------------------------------------------------------------

#[test]
fn non_capturing_overlay_renders_even_though_it_does_not_capture_input() {
    let terminal = VirtualTerminal::new(40, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    struct Overlay;
    impl Component for Overlay {
        impl_component_any!();
        fn render(&mut self, _width: usize) -> Vec<String> {
            vec!["VISIBLE".to_string()]
        }
    }

    tui.root.add_child(Box::new(StaticLines::new(["base"])));
    tui.show_overlay(
        Box::new(Overlay),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(10)),
            non_capturing: true,
            ..Default::default()
        },
    );
    wait_for_render(&mut tui);

    assert!(
        terminal.viewport()[0].contains("VISIBLE"),
        "non-capturing overlay should still be composited; got {:?}",
        terminal.viewport()[0],
    );
}

// ---------------------------------------------------------------------------
// Auto-focus on capturing show, and focus restoration on hide
// ---------------------------------------------------------------------------

#[test]
fn capturing_overlay_auto_focuses_on_show_and_unfocuses_root() {
    // The default overlay is capturing and should grab focus immediately,
    // matching the "modal dialog takes input until dismissed" intuition.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(Box::new(overlay), OverlayOptions::default());

    assert!(
        tui.is_overlay_focused(&handle),
        "capturing overlay should be focused on show",
    );
    assert!(focused(&overlay_state));
    assert!(!focused(&editor_state), "root child should lose focus");
}

#[test]
fn hiding_a_focused_non_capturing_overlay_restores_root_focus() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );
    tui.focus_overlay(&handle);
    tui.hide_overlay(&handle);
    wait_for_render(&mut tui);

    assert!(focused(&editor_state), "root focus should be restored");
    assert!(!focused(&overlay_state));
}

#[test]
fn hiding_a_non_focused_overlay_does_not_change_focus() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );
    // Never focused. Removing it should be a no-op on the focus state.
    tui.hide_overlay(&handle);
    wait_for_render(&mut tui);

    assert!(focused(&editor_state));
    assert!(!focused(&overlay_state));
}

#[test]
fn capturing_overlay_unfocus_falls_back_to_pre_focus() {
    // Mirror of the "unfocus on a capturing overlay restores the editor"
    // scenario: auto-focus on show, then explicit unfocus should undo
    // the capture and re-focus the root child that had focus before.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let capturing = FocusableRecorder::new();
    let capturing_state = capturing.handle();
    let handle = tui.show_overlay(Box::new(capturing), OverlayOptions::default());
    assert!(focused(&capturing_state), "auto-focused on show");

    tui.unfocus_overlay(&handle);
    wait_for_render(&mut tui);

    assert!(focused(&editor_state));
    assert!(!focused(&capturing_state));
}

#[test]
fn capturing_overlay_removed_with_non_capturing_below_restores_root_focus() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let non_capturing = FocusableRecorder::new();
    let non_capturing_state = non_capturing.handle();
    tui.show_overlay(
        Box::new(non_capturing),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    let capturing = FocusableRecorder::new();
    let capturing_state = capturing.handle();
    let handle = tui.show_overlay(Box::new(capturing), OverlayOptions::default());
    assert!(focused(&capturing_state), "capturing overlay auto-focused");

    // Removing the capturing overlay: the non-capturing one is not a
    // valid promotion target, so focus falls back to the editor.
    tui.hide_overlay(&handle);
    wait_for_render(&mut tui);

    assert!(focused(&editor_state));
    assert!(!focused(&non_capturing_state));
    assert!(!focused(&capturing_state));
}

#[test]
fn multiple_capturing_and_non_capturing_restore_focus_through_removals() {
    // Sequence: c1, n1, c2, n2. c2 is focused (auto via show). Removing
    // c2 promotes to c1; removing c1 restores editor focus.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let c1 = FocusableRecorder::new();
    let c1_state = c1.handle();
    let c1_handle = tui.show_overlay(Box::new(c1), OverlayOptions::default());

    tui.show_overlay(
        Box::new(FocusableRecorder::new()),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    let c2 = FocusableRecorder::new();
    let c2_state = c2.handle();
    let c2_handle = tui.show_overlay(Box::new(c2), OverlayOptions::default());

    tui.show_overlay(
        Box::new(FocusableRecorder::new()),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    assert!(focused(&c2_state), "c2 (most recent capturing) has focus");

    tui.hide_overlay(&c2_handle);
    wait_for_render(&mut tui);
    assert!(
        focused(&c1_state),
        "focus should promote to c1 (next-topmost capturing)"
    );
    assert!(!focused(&c2_state));

    tui.hide_overlay(&c1_handle);
    wait_for_render(&mut tui);
    assert!(
        focused(&editor_state),
        "no capturing overlays remain; root focus restored",
    );
    assert!(!focused(&c1_state));
}

// ---------------------------------------------------------------------------
// set_overlay_hidden variants
// ---------------------------------------------------------------------------

#[test]
fn set_overlay_hidden_false_on_non_capturing_does_not_auto_focus() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    tui.set_overlay_hidden(&handle, true);
    tui.set_overlay_hidden(&handle, false);
    wait_for_render(&mut tui);

    assert!(focused(&editor_state));
    assert!(
        !focused(&overlay_state),
        "unhiding a non-capturing overlay should not auto-focus it",
    );
}

// ---------------------------------------------------------------------------
// visible callback
// ---------------------------------------------------------------------------

#[test]
fn input_routing_skips_capturing_overlay_whose_visible_callback_returns_false() {
    // Layout: root(editor) + capturing fallback + non-capturing overlay
    // + capturing "primary" gated on a toggleable `visible` callback.
    // Once primary goes invisible, input must fall through to the
    // fallback capturing overlay — NOT to the non-capturing one.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let fallback = FocusableRecorder::new();
    let fallback_state = fallback.handle();
    tui.show_overlay(Box::new(fallback), OverlayOptions::default());

    let nc = FocusableRecorder::new();
    let nc_state = nc.handle();
    tui.show_overlay(
        Box::new(nc),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    let primary_visible = Rc::new(Cell::new(true));
    let primary = FocusableRecorder::new();
    let primary_state = primary.handle();
    let primary_flag = Rc::clone(&primary_visible);
    tui.show_overlay(
        Box::new(primary),
        OverlayOptions {
            visible: Some(Box::new(move |_, _| primary_flag.get())),
            ..Default::default()
        },
    );
    // Flip primary to invisible; its auto-focus state gets overridden by
    // the routing's visible-filter.
    primary_visible.set(false);

    tui.handle_input(&Key::char('x'));

    assert_eq!(
        input_count(&primary_state),
        0,
        "invisible primary should not receive input",
    );
    assert_eq!(
        input_count(&nc_state),
        0,
        "non-capturing overlay must never receive input via routing",
    );
    assert_eq!(
        input_count(&fallback_state),
        1,
        "input should fall through to the next-topmost capturing overlay",
    );
}

#[test]
fn invisible_focused_overlay_heals_focus_to_topmost_visible_capturing_on_input() {
    // Regression guard for the `handle_input` focus-heal step.
    //
    // When the currently-focused overlay becomes invisible between
    // frames (e.g. a `visible` callback flips off because a terminal
    // resize dropped below its min-width threshold, or because
    // application state changed), the next input dispatch must:
    //
    //   1. Route the event to the topmost visible *capturing*
    //      overlay, skipping any non-capturing overlays that happen
    //      to sit above it in the stack.
    //   2. *Also* transfer focus state (`is_focused()`, `set_focused`
    //      side effects) to that fallback capturing overlay, so a
    //      subsequent render sees the new focus holder as the source
    //      of truth.
    //
    // Our existing
    // `input_routing_skips_capturing_overlay_whose_visible_callback_returns_false`
    // test covers (1); this test covers (2).
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let fallback = FocusableRecorder::new();
    let fallback_state = fallback.handle();
    tui.show_overlay(Box::new(fallback), OverlayOptions::default());
    // `fallback` is focused by auto-focus-on-show.
    assert!(focused(&fallback_state));

    let nc = FocusableRecorder::new();
    let nc_state = nc.handle();
    tui.show_overlay(
        Box::new(nc),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    let primary_visible = Rc::new(Cell::new(true));
    let primary = FocusableRecorder::new();
    let primary_state = primary.handle();
    let primary_flag = Rc::clone(&primary_visible);
    tui.show_overlay(
        Box::new(primary),
        OverlayOptions {
            visible: Some(Box::new(move |_, _| primary_flag.get())),
            ..Default::default()
        },
    );
    // primary auto-focuses on show: fallback loses focus, primary gains.
    assert!(focused(&primary_state));
    assert!(!focused(&fallback_state));

    // Flip primary invisible; at the next input dispatch the focus
    // heal step must kick in.
    primary_visible.set(false);
    tui.handle_input(&Key::char('x'));

    // fallback (topmost visible capturing) now holds focus.
    assert!(
        focused(&fallback_state),
        "fallback must regain focus after primary becomes invisible",
    );
    assert!(
        !focused(&primary_state),
        "primary must lose focus once invisible",
    );
    // Non-capturing nc is *never* a focus target via the heal step,
    // even though it sits above fallback in the stack.
    assert!(
        !focused(&nc_state),
        "non-capturing overlay must not gain focus"
    );
    // Editor stays unfocused — it only regains focus if there are no
    // visible capturing overlays at all.
    assert!(!focused(&editor_state));
}

// ---------------------------------------------------------------------------
// No-op guards
// ---------------------------------------------------------------------------

#[test]
fn focus_overlay_on_hidden_overlay_is_a_noop() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );
    tui.set_overlay_hidden(&handle, true);
    tui.focus_overlay(&handle);
    wait_for_render(&mut tui);

    assert!(focused(&editor_state));
    assert!(!focused(&overlay_state));
    assert!(!tui.is_overlay_focused(&handle));
}

#[test]
fn unfocus_overlay_when_not_focused_is_a_noop() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    let handle = tui.show_overlay(
        Box::new(overlay),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );
    // Overlay was never focused; this should not disturb anything.
    tui.unfocus_overlay(&handle);
    wait_for_render(&mut tui);

    assert!(focused(&editor_state));
    assert!(!focused(&overlay_state));
}

#[test]
fn unfocus_overlay_with_no_saved_focus_clears_focus_state_entirely() {
    // When the overlay was shown before any root child had focus, the
    // saved pre-focus is `FocusTarget::None`. Unfocusing must reset to
    // that state (no focused child) without routing input back to the
    // overlay.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let overlay = FocusableRecorder::new();
    let overlay_state = overlay.handle();
    // No root child; no prior focus.
    let handle = tui.show_overlay(Box::new(overlay), OverlayOptions::default());
    assert!(focused(&overlay_state), "auto-focus on capturing show");

    tui.unfocus_overlay(&handle);
    assert!(!focused(&overlay_state));
    // Input after unfocus: no one receives it.
    tui.handle_input(&Key::char('x'));
    assert_eq!(
        input_count(&overlay_state),
        0,
        "unfocused overlay should not receive input",
    );
    assert!(!tui.is_overlay_focused(&handle));
}

// ---------------------------------------------------------------------------
// Focus cycle prevention
// ---------------------------------------------------------------------------

#[test]
fn toggle_focus_between_non_capturing_then_unfocus_returns_to_root() {
    // Saving the pre-focus target only on the *first* focus transfer
    // (not subsequent ones) keeps the cycle: focus a → focus b →
    // focus a → unfocus a should land back on the editor, not on b.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let a = FocusableRecorder::new();
    let a_state = a.handle();
    let a_handle = tui.show_overlay(
        Box::new(a),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );
    let b = FocusableRecorder::new();
    let b_state = b.handle();
    let b_handle = tui.show_overlay(
        Box::new(b),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    tui.focus_overlay(&a_handle);
    tui.focus_overlay(&b_handle);
    tui.focus_overlay(&a_handle);
    tui.unfocus_overlay(&a_handle);
    wait_for_render(&mut tui);

    assert!(focused(&editor_state));
    assert!(!focused(&a_state));
    assert!(!focused(&b_state));
}

// ---------------------------------------------------------------------------
// hide_topmost_overlay semantics
// ---------------------------------------------------------------------------

#[test]
fn hide_topmost_overlay_is_noop_when_top_is_non_capturing() {
    // Stack (bottom→top): capturing, then non-capturing. hide_topmost
    // targets the topmost *overall* overlay, and is a no-op if that is
    // non-capturing (backdrop layers should not be dismissed by Esc).
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let capturing = FocusableRecorder::new();
    let capturing_state = capturing.handle();
    tui.show_overlay(Box::new(capturing), OverlayOptions::default());

    let non_capturing = FocusableRecorder::new();
    tui.show_overlay(
        Box::new(non_capturing),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    assert!(
        focused(&capturing_state),
        "capturing overlay is auto-focused"
    );

    // Topmost (by z-order) is the non-capturing overlay → hide_topmost
    // is a no-op and capturing remains focused.
    assert!(!tui.hide_topmost_overlay());
    wait_for_render(&mut tui);
    assert!(focused(&capturing_state));
}

#[test]
fn hide_topmost_overlay_removes_a_capturing_overlay_and_restores_focus() {
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let capturing = FocusableRecorder::new();
    let capturing_state = capturing.handle();
    tui.show_overlay(Box::new(capturing), OverlayOptions::default());
    assert!(focused(&capturing_state));

    assert!(tui.hide_topmost_overlay());
    wait_for_render(&mut tui);

    assert!(focused(&editor_state));
    assert!(!focused(&capturing_state));
}

#[test]
fn microtask_deferred_sub_overlay_pattern_restores_focus_on_teardown() {
    // Regression guard for a focus-management sequence that arises when
    // an extension shows a non-capturing backdrop synchronously from
    // inside a callback and then shows a capturing controller via a
    // deferred continuation (microtask-style).
    //
    // In this crate all overlay operations are synchronous — the focus
    // state machine doesn't depend on task-scheduling order — so we
    // drive the equivalent operation sequence directly:
    //
    //   1. Editor focused.
    //   2. Show non-capturing `timer` — editor stays focused.
    //   3. Show capturing `controller` — saves editor as pre-focus,
    //      auto-focuses the controller.
    //   4. "Done" callback: hide `timer` (not focused, no focus
    //      change), then hide topmost capturing (`controller`) —
    //      promotion has no other capturing overlay to jump to, so
    //      the saved pre-focus (editor) is restored.
    //
    // After step 4, the editor must regain focus and subsequent
    // input must route to it rather than any stale overlay.
    let terminal = VirtualTerminal::new(40, 8);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    let editor_state = editor.handle();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let timer = FocusableRecorder::new();
    let timer_state = timer.handle();
    let timer_handle = tui.show_overlay(
        Box::new(timer),
        OverlayOptions {
            non_capturing: true,
            ..Default::default()
        },
    );

    let controller = FocusableRecorder::new();
    let controller_state = controller.handle();
    let _controller_handle = tui.show_overlay(Box::new(controller), OverlayOptions::default());

    // Mid-sequence: controller has focus, editor doesn't.
    assert!(focused(&controller_state));
    assert!(!focused(&editor_state));
    assert!(!focused(&timer_state));

    // "Done" step: tear down the sub-overlays. Order matters (timer
    // hides first, then the topmost capturing controller) but both
    // sequences must end with the editor focused.
    tui.hide_overlay(&timer_handle);
    assert!(tui.hide_topmost_overlay());
    wait_for_render(&mut tui);

    // Editor regained focus, no stale overlay state.
    assert!(
        focused(&editor_state),
        "editor regains focus after teardown"
    );
    assert!(!focused(&controller_state));
    assert!(!focused(&timer_state));

    // Follow-up input reaches the editor, not the dismissed overlays.
    tui.handle_input(&Key::char('x'));
    assert_eq!(
        input_count(&editor_state),
        1,
        "editor receives input after teardown",
    );
    assert_eq!(input_count(&controller_state), 0);
    assert_eq!(input_count(&timer_state), 0);
}

// ---------------------------------------------------------------------------
// Rendering order
// ---------------------------------------------------------------------------

struct SingleChar(&'static str);
impl Component for SingleChar {
    impl_component_any!();
    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![self.0.to_string()]
    }
}

fn overlay_at_origin(label: &'static str) -> (Box<SingleChar>, OverlayOptions) {
    (
        Box::new(SingleChar(label)),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(1)),
            row: Some(SizeValue::Absolute(0)),
            col: Some(SizeValue::Absolute(0)),
            non_capturing: true,
            ..Default::default()
        },
    )
}

fn top_char(terminal: &VirtualTerminal) -> char {
    terminal.viewport()[0].chars().next().unwrap_or(' ')
}

#[test]
fn default_rendering_order_for_overlapping_overlays_follows_creation() {
    let terminal = VirtualTerminal::new(20, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.root.add_child(Box::new(StaticLines::new([""])));

    let (a, a_opts) = overlay_at_origin("A");
    tui.show_overlay(a, a_opts);
    let (b, b_opts) = overlay_at_origin("B");
    tui.show_overlay(b, b_opts);
    wait_for_render(&mut tui);

    assert_eq!(
        top_char(&terminal),
        'B',
        "the overlay created last renders on top",
    );
}

#[test]
fn focus_overlay_on_lower_overlay_renders_it_on_top() {
    let terminal = VirtualTerminal::new(20, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.root.add_child(Box::new(StaticLines::new([""])));

    let (a, a_opts) = overlay_at_origin("A");
    let a_handle = tui.show_overlay(a, a_opts);
    let (b, b_opts) = overlay_at_origin("B");
    tui.show_overlay(b, b_opts);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'B');

    // Focus the lower overlay; its z-order bump should bring it forward.
    tui.focus_overlay(&a_handle);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'A');
}

#[test]
fn focus_overlay_on_already_focused_still_bumps_its_visual_order() {
    let terminal = VirtualTerminal::new(20, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.root.add_child(Box::new(StaticLines::new([""])));

    let (a, a_opts) = overlay_at_origin("A");
    let a_handle = tui.show_overlay(a, a_opts);
    tui.show_overlay(
        Box::new(SingleChar("B")),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(1)),
            row: Some(SizeValue::Absolute(0)),
            col: Some(SizeValue::Absolute(0)),
            non_capturing: true,
            ..Default::default()
        },
    );
    tui.focus_overlay(&a_handle);
    tui.show_overlay(
        Box::new(SingleChar("C")),
        OverlayOptions {
            anchor: OverlayAnchor::TopLeft,
            width: Some(SizeValue::Absolute(1)),
            row: Some(SizeValue::Absolute(0)),
            col: Some(SizeValue::Absolute(0)),
            non_capturing: true,
            ..Default::default()
        },
    );
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'C');

    // Re-focusing 'a' should bring it in front of 'c' as well.
    tui.focus_overlay(&a_handle);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'A');
    assert!(tui.is_overlay_focused(&a_handle));
}

#[test]
fn focusing_middle_overlay_places_it_on_top_preserving_others() {
    let terminal = VirtualTerminal::new(20, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.root.add_child(Box::new(StaticLines::new([""])));

    let (a, a_opts) = overlay_at_origin("A");
    tui.show_overlay(a, a_opts);
    let (b, b_opts) = overlay_at_origin("B");
    let b_handle = tui.show_overlay(b, b_opts);
    let (c, c_opts) = overlay_at_origin("C");
    let c_handle = tui.show_overlay(c, c_opts);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'C');

    // Focus the middle overlay: it surfaces above the previously-top C.
    tui.focus_overlay(&b_handle);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'B');

    // Hiding B reveals whatever was next in z-order, which is C.
    tui.hide_overlay(&b_handle);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'C');

    // Hiding C reveals A (the lowest remaining).
    tui.hide_overlay(&c_handle);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'A');
}

#[test]
fn unfocus_overlay_does_not_change_visual_order_until_another_is_focused() {
    let terminal = VirtualTerminal::new(20, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    let editor = FocusableRecorder::new();
    tui.root.add_child(Box::new(editor));
    tui.set_focus(Some(0));

    let (a, a_opts) = overlay_at_origin("A");
    let a_handle = tui.show_overlay(a, a_opts);
    let (b, b_opts) = overlay_at_origin("B");
    let b_handle = tui.show_overlay(b, b_opts);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'B');

    tui.focus_overlay(&a_handle);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'A');

    // Unfocusing A restores the editor's focus but must NOT send A
    // back to the bottom of the stack: the visual-ordering invariant
    // tracks focus_order, not the current focus holder.
    tui.unfocus_overlay(&a_handle);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'A');

    // Focusing B brings it forward again.
    tui.focus_overlay(&b_handle);
    wait_for_render(&mut tui);
    assert_eq!(top_char(&terminal), 'B');
}

#[test]
fn capturing_overlay_hidden_and_shown_again_renders_on_top() {
    // Regression guard for the `setHidden(false)` z-order bump on
    // capturing overlays.
    //
    // Scenario:
    //   1. Show non-capturing `A` at (0, 0), width 1.
    //   2. Show capturing `B` at the same spot — `B` renders on top.
    //   3. Hide `B`. At this point `B` is out of the composite, and
    //      the only visible overlay at (0, 0) is `A`.
    //   4. Show non-capturing `C` at (0, 0). `C` now has the highest
    //      focus_order among visible overlays, so it paints on top.
    //   5. Unhide `B`. Upstream bumps `B`'s focus_order so it
    //      reclaims the top of the visual stack; without the bump,
    //      `C` would still be on top and the user would see the
    //      "wrong" overlay after a hide/unhide cycle.
    let terminal = VirtualTerminal::new(20, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.root.add_child(Box::new(StaticLines::new([""])));

    let (a, a_opts) = overlay_at_origin("A");
    tui.show_overlay(a, a_opts);

    // Override the non_capturing flag on B so it's a capturing overlay.
    let (b, mut b_opts) = overlay_at_origin("B");
    b_opts.non_capturing = false;
    let b_handle = tui.show_overlay(b, b_opts);
    wait_for_render(&mut tui);
    assert_eq!(
        top_char(&terminal),
        'B',
        "capturing B should render on top when first shown; got {:?}",
        terminal.viewport(),
    );

    // Hide B, then show non-capturing C at the same location.
    tui.set_overlay_hidden(&b_handle, true);
    let (c, c_opts) = overlay_at_origin("C");
    tui.show_overlay(c, c_opts);
    wait_for_render(&mut tui);
    assert_eq!(
        top_char(&terminal),
        'C',
        "while B is hidden, newly-shown C owns the top of the stack; got {:?}",
        terminal.viewport(),
    );

    // Unhide B. It must reclaim the top — `set_overlay_hidden(false)`
    // bumps z-order on capturing overlays so the newly-visible entry
    // wins over anything that opened while it was hidden.
    tui.set_overlay_hidden(&b_handle, false);
    wait_for_render(&mut tui);
    assert_eq!(
        top_char(&terminal),
        'B',
        "unhiding B must restore it to the top of the visual stack; got {:?}",
        terminal.viewport(),
    );
}
