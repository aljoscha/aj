//! Tests for `Component::wants_key_release` dispatch gating.
//!
//! The Kitty keyboard protocol — and `crossterm` when
//! `REPORT_EVENT_TYPES` is active — delivers both press and release
//! events for every key. `Tui::handle_input` filters out releases
//! before routing the event to the focused component, unless that
//! component opts in by returning `true` from
//! [`Component::wants_key_release`].
//!
//! These tests cover the three dispatch sites that `Tui::handle_input`
//! routes through: the focused root child, a routing-pool overlay, and
//! an explicitly focused overlay. All three must honor the gate.
//!
//! Key-repeat events (`KeyEventKind::Repeat`) are always delivered; a
//! regression guard covers that too.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::impl_component_any;
use aj_tui::keys::InputEvent;
use aj_tui::tui::{OverlayOptions, Tui};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

use support::VirtualTerminal;

// ---------------------------------------------------------------------------
// Fixture: component that records the `kind` of every key event it
// receives, optionally opting in to releases.
// ---------------------------------------------------------------------------

struct Recorder {
    kinds: Rc<RefCell<Vec<KeyEventKind>>>,
    wants_release: bool,
}

impl Recorder {
    fn new(wants_release: bool) -> (Self, Rc<RefCell<Vec<KeyEventKind>>>) {
        let kinds = Rc::new(RefCell::new(Vec::new()));
        (
            Self {
                kinds: Rc::clone(&kinds),
                wants_release,
            },
            kinds,
        )
    }
}

impl Component for Recorder {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        Vec::new()
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        if let InputEvent::Key(k) = event {
            self.kinds.borrow_mut().push(k.kind);
        }
        true
    }

    fn wants_key_release(&self) -> bool {
        self.wants_release
    }
}

// ---------------------------------------------------------------------------
// Event constructors
// ---------------------------------------------------------------------------

fn press(c: char) -> InputEvent {
    InputEvent::Key(KeyEvent {
        code: KeyCode::Char(c),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: KeyEventState::NONE,
    })
}

fn release(c: char) -> InputEvent {
    InputEvent::Key(KeyEvent {
        code: KeyCode::Char(c),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Release,
        state: KeyEventState::NONE,
    })
}

fn repeat(c: char) -> InputEvent {
    InputEvent::Key(KeyEvent {
        code: KeyCode::Char(c),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Repeat,
        state: KeyEventState::NONE,
    })
}

// ---------------------------------------------------------------------------
// Default behavior: focused root child drops releases
// ---------------------------------------------------------------------------

#[test]
fn default_component_does_not_see_release_events_on_the_focused_root_child() {
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal));
    let (recorder, kinds) = Recorder::new(false);
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));

    tui.handle_input(&press('a'));
    tui.handle_input(&release('a'));
    tui.handle_input(&press('b'));

    assert_eq!(
        *kinds.borrow(),
        vec![KeyEventKind::Press, KeyEventKind::Press],
        "release must be filtered when wants_key_release is false",
    );
}

#[test]
fn opted_in_component_sees_release_events_on_the_focused_root_child() {
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal));
    let (recorder, kinds) = Recorder::new(true);
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));

    tui.handle_input(&press('a'));
    tui.handle_input(&release('a'));

    assert_eq!(
        *kinds.borrow(),
        vec![KeyEventKind::Press, KeyEventKind::Release],
        "opted-in component must see both Press and Release",
    );
}

// ---------------------------------------------------------------------------
// Repeats always flow through regardless of the opt-in flag
// ---------------------------------------------------------------------------

#[test]
fn repeat_events_are_delivered_regardless_of_wants_key_release() {
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal));
    let (recorder, kinds) = Recorder::new(false);
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));

    tui.handle_input(&press('a'));
    tui.handle_input(&repeat('a'));
    tui.handle_input(&repeat('a'));

    assert_eq!(
        *kinds.borrow(),
        vec![
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Repeat,
        ],
        "repeats must always be delivered; only releases are gated",
    );
}

// ---------------------------------------------------------------------------
// Overlay dispatch sites: routing pool + explicit focus
// ---------------------------------------------------------------------------

#[test]
fn routing_pool_overlay_honors_the_gate() {
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal));

    let (default, default_kinds) = Recorder::new(false);
    let _handle = tui.show_overlay(Box::new(default), OverlayOptions::default());

    tui.handle_input(&press('a'));
    tui.handle_input(&release('a'));

    assert_eq!(
        *default_kinds.borrow(),
        vec![KeyEventKind::Press],
        "routing-pool overlay without opt-in should drop releases",
    );
}

#[test]
fn routing_pool_overlay_receives_releases_when_opted_in() {
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal));

    let (opted, opted_kinds) = Recorder::new(true);
    let _handle = tui.show_overlay(Box::new(opted), OverlayOptions::default());

    tui.handle_input(&press('a'));
    tui.handle_input(&release('a'));

    assert_eq!(
        *opted_kinds.borrow(),
        vec![KeyEventKind::Press, KeyEventKind::Release],
        "opted-in routing-pool overlay should see both edges",
    );
}

#[test]
fn explicitly_focused_overlay_honors_the_gate() {
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal));

    let (default, default_kinds) = Recorder::new(false);
    let handle = tui.show_overlay(Box::new(default), OverlayOptions::default());
    tui.focus_overlay(&handle);

    tui.handle_input(&press('a'));
    tui.handle_input(&release('a'));

    assert_eq!(
        *default_kinds.borrow(),
        vec![KeyEventKind::Press],
        "explicitly-focused overlay without opt-in should drop releases",
    );
}

#[test]
fn explicitly_focused_overlay_receives_releases_when_opted_in() {
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal));

    let (opted, opted_kinds) = Recorder::new(true);
    let handle = tui.show_overlay(Box::new(opted), OverlayOptions::default());
    tui.focus_overlay(&handle);

    tui.handle_input(&press('a'));
    tui.handle_input(&release('a'));

    assert_eq!(
        *opted_kinds.borrow(),
        vec![KeyEventKind::Press, KeyEventKind::Release],
        "opted-in explicitly-focused overlay should see both edges",
    );
}

// ---------------------------------------------------------------------------
// InputEvent helpers (used both by Tui internals and by test assertions)
// ---------------------------------------------------------------------------

#[test]
fn is_key_release_and_is_key_repeat_discriminate_correctly() {
    let p = press('x');
    let r = release('x');
    let rp = repeat('x');

    assert!(!p.is_key_release());
    assert!(!p.is_key_repeat());

    assert!(r.is_key_release());
    assert!(!r.is_key_repeat());

    assert!(!rp.is_key_release());
    assert!(rp.is_key_repeat());

    // Non-Key events never look like press/repeat/release.
    let paste = InputEvent::Paste("hi".to_string());
    assert!(!paste.is_key_release());
    assert!(!paste.is_key_repeat());

    let resize = InputEvent::Resize(80, 24);
    assert!(!resize.is_key_release());
    assert!(!resize.is_key_repeat());
}
