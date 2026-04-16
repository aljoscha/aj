//! Tests for `Tui::set_on_debug`: the global `Shift+Ctrl+D` hook that
//! fires before input routing and consumes the chord so components
//! never see it.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use aj_tui::keys::{InputEvent, Key};
use aj_tui::tui::Tui;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use support::{InputRecorder, VirtualTerminal};

fn shift_ctrl_d() -> InputEvent {
    // Lowercase char with SHIFT is the canonical crossterm encoding
    // for a shift+ctrl+letter chord (terminals send Ctrl+D regardless
    // of Shift, and crossterm reports it with SHIFT still in the
    // modifier set).
    InputEvent::Key(KeyEvent::new(
        KeyCode::Char('d'),
        KeyModifiers::SHIFT | KeyModifiers::CONTROL,
    ))
}

#[test]
fn debug_hook_fires_on_shift_ctrl_d() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal));
    let hits = Rc::new(RefCell::new(0_u32));
    let hits_clone = Rc::clone(&hits);
    tui.set_on_debug(move || {
        *hits_clone.borrow_mut() += 1;
    });

    tui.handle_input(&shift_ctrl_d());
    assert_eq!(*hits.borrow(), 1);

    tui.handle_input(&shift_ctrl_d());
    assert_eq!(*hits.borrow(), 2);
}

#[test]
fn debug_hook_consumes_the_event_so_components_do_not_see_it() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal));
    let (recorder, events) = InputRecorder::new();
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));

    tui.set_on_debug(|| {});

    tui.handle_input(&shift_ctrl_d());
    assert!(
        events.borrow().is_empty(),
        "component must not see Shift+Ctrl+D when a debug hook is registered; got {:?}",
        events.borrow(),
    );
}

#[test]
fn other_keys_pass_through_when_debug_hook_is_registered() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal));
    let (recorder, events) = InputRecorder::new();
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));
    tui.set_on_debug(|| {});

    tui.handle_input(&Key::char('a'));
    tui.handle_input(&Key::enter());

    let received = events.borrow();
    assert_eq!(received.len(), 2, "non-debug events must still route");
}

#[test]
fn without_a_hook_shift_ctrl_d_reaches_components() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal));
    let (recorder, events) = InputRecorder::new();
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));

    tui.handle_input(&shift_ctrl_d());
    assert_eq!(
        events.borrow().len(),
        1,
        "with no debug hook, Shift+Ctrl+D is a normal input",
    );
}

#[test]
fn clear_on_debug_lets_components_see_the_chord_again() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal));
    let (recorder, events) = InputRecorder::new();
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));

    let hits = Rc::new(RefCell::new(0_u32));
    let hits_clone = Rc::clone(&hits);
    tui.set_on_debug(move || {
        *hits_clone.borrow_mut() += 1;
    });

    tui.handle_input(&shift_ctrl_d());
    assert_eq!(*hits.borrow(), 1);
    assert_eq!(events.borrow().len(), 0);

    tui.clear_on_debug();
    tui.handle_input(&shift_ctrl_d());
    assert_eq!(*hits.borrow(), 1, "callback must not fire after clear");
    assert_eq!(
        events.borrow().len(),
        1,
        "after clear, Shift+Ctrl+D reaches components again",
    );
}

#[test]
fn set_on_debug_replaces_previous_callback() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal));

    let first = Rc::new(RefCell::new(0_u32));
    let second = Rc::new(RefCell::new(0_u32));

    let first_clone = Rc::clone(&first);
    tui.set_on_debug(move || *first_clone.borrow_mut() += 1);

    let second_clone = Rc::clone(&second);
    tui.set_on_debug(move || *second_clone.borrow_mut() += 1);

    tui.handle_input(&shift_ctrl_d());
    assert_eq!(*first.borrow(), 0, "first callback was replaced");
    assert_eq!(*second.borrow(), 1, "second callback fired");
}
