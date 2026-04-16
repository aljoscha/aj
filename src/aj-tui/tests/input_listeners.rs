//! Tests for `Tui::add_input_listener` / `Tui::remove_input_listener`.
//!
//! Listeners are a pre-component interception hook: they run in insertion
//! order on every `Tui::handle_input` call, before any overlay / focus
//! routing. They can pass the event through, rewrite it, or consume it
//! entirely. These tests cover the five invariants the listener API
//! guarantees:
//!
//! 1. Listeners see the event before the focused component does.
//! 2. `Consume` stops dispatch so the focused component never sees it.
//! 3. `Rewrite` replaces the event for subsequent listeners and the
//!    dispatch path.
//! 4. Removing a listener preserves the relative order of the rest.
//! 5. Two listeners chain: the second observes the first's rewrite.

mod support;

use std::cell::RefCell;
use std::rc::Rc;

use aj_tui::keys::{InputEvent, Key};
use aj_tui::tui::{InputListenerAction, Tui};

use crossterm::event::{KeyCode, KeyEvent};

use support::{InputRecorder, VirtualTerminal, send_keys};

fn setup() -> (Tui, Rc<RefCell<Vec<InputEvent>>>) {
    let terminal = VirtualTerminal::new(20, 5);
    let mut tui = Tui::new(Box::new(terminal));
    let (recorder, events) = InputRecorder::new();
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));
    (tui, events)
}

// ---------------------------------------------------------------------------
// (1) Listener sees event before the focused component
// ---------------------------------------------------------------------------

#[test]
fn listener_sees_event_before_focused_component() {
    let (mut tui, events) = setup();
    let seen = Rc::new(RefCell::new(Vec::<InputEvent>::new()));
    let seen_inner = Rc::clone(&seen);

    tui.add_input_listener(move |event| {
        seen_inner.borrow_mut().push(event.clone());
        InputListenerAction::Pass
    });

    send_keys(&mut tui, [Key::char('h'), Key::char('i')]);

    assert_eq!(seen.borrow().len(), 2, "listener saw both events");
    assert_eq!(
        events.borrow().len(),
        2,
        "focused component also saw both events",
    );
    // Order matches insertion order: listener ran first, then the
    // component. We can't assert strict temporal order from the log alone,
    // but we can assert both received the same payloads in the same
    // order, which is enough to characterize Pass semantics.
    for (a, b) in seen.borrow().iter().zip(events.borrow().iter()) {
        match (a, b) {
            (InputEvent::Key(ka), InputEvent::Key(kb)) => assert_eq!(ka.code, kb.code),
            _ => panic!("expected two key events"),
        }
    }
}

// ---------------------------------------------------------------------------
// (2) Consume stops dispatch
// ---------------------------------------------------------------------------

#[test]
fn consume_stops_dispatch_to_focused_component() {
    let (mut tui, events) = setup();

    // Drop every event at the listener.
    tui.add_input_listener(|_| InputListenerAction::Consume);

    send_keys(&mut tui, [Key::char('h'), Key::char('i')]);

    assert!(
        events.borrow().is_empty(),
        "consumed events must not reach the focused component",
    );
}

#[test]
fn consume_short_circuits_subsequent_listeners() {
    let (mut tui, _events) = setup();
    let second_ran = Rc::new(RefCell::new(false));
    let second_inner = Rc::clone(&second_ran);

    tui.add_input_listener(|_| InputListenerAction::Consume);
    tui.add_input_listener(move |_| {
        *second_inner.borrow_mut() = true;
        InputListenerAction::Pass
    });

    send_keys(&mut tui, [Key::char('x')]);

    assert!(
        !*second_ran.borrow(),
        "second listener must not run after the first consumes",
    );
}

// ---------------------------------------------------------------------------
// (3) Rewrite replaces the event
// ---------------------------------------------------------------------------

#[test]
fn rewrite_replaces_event_seen_by_focused_component() {
    let (mut tui, events) = setup();

    tui.add_input_listener(|_| InputListenerAction::Rewrite(Key::char('Z')));

    send_keys(&mut tui, [Key::char('a')]);

    let log = events.borrow();
    assert_eq!(log.len(), 1);
    assert!(matches!(
        log[0],
        InputEvent::Key(KeyEvent {
            code: KeyCode::Char('Z'),
            ..
        })
    ));
}

// ---------------------------------------------------------------------------
// (4) Removing a listener preserves order
// ---------------------------------------------------------------------------

#[test]
fn remove_input_listener_preserves_order_of_the_rest() {
    let (mut tui, events) = setup();
    let log = Rc::new(RefCell::new(Vec::<&'static str>::new()));

    let log_a = Rc::clone(&log);
    let h_a = tui.add_input_listener(move |_| {
        log_a.borrow_mut().push("a");
        InputListenerAction::Pass
    });
    let log_b = Rc::clone(&log);
    let _h_b = tui.add_input_listener(move |_| {
        log_b.borrow_mut().push("b");
        InputListenerAction::Pass
    });
    let log_c = Rc::clone(&log);
    let _h_c = tui.add_input_listener(move |_| {
        log_c.borrow_mut().push("c");
        InputListenerAction::Pass
    });

    send_keys(&mut tui, [Key::char('x')]);
    assert_eq!(*log.borrow(), vec!["a", "b", "c"]);

    log.borrow_mut().clear();

    tui.remove_input_listener(h_a);
    send_keys(&mut tui, [Key::char('y')]);
    assert_eq!(
        *log.borrow(),
        vec!["b", "c"],
        "removing the first listener leaves the rest in order",
    );

    // The focused component still received both key events.
    assert_eq!(events.borrow().len(), 2);
}

#[test]
fn remove_input_listener_ignores_unknown_handles() {
    let (mut tui, _events) = setup();
    let h = tui.add_input_listener(|_| InputListenerAction::Pass);
    // Remove once — fine.
    tui.remove_input_listener(h);
    // Remove again — should be a silent no-op (idempotent).
    tui.remove_input_listener(h);
}

// ---------------------------------------------------------------------------
// (5) Chained listeners
// ---------------------------------------------------------------------------

#[test]
fn chained_listeners_see_the_previous_listeners_rewrite() {
    let (mut tui, events) = setup();
    let second_saw = Rc::new(RefCell::new(Vec::<InputEvent>::new()));
    let second_inner = Rc::clone(&second_saw);

    // First listener rewrites every event to 'Z'.
    tui.add_input_listener(|_| InputListenerAction::Rewrite(Key::char('Z')));
    // Second listener just records what it sees and passes through.
    tui.add_input_listener(move |event| {
        second_inner.borrow_mut().push(event.clone());
        InputListenerAction::Pass
    });

    send_keys(&mut tui, [Key::char('a')]);

    let observed = second_saw.borrow();
    assert_eq!(observed.len(), 1);
    assert!(
        matches!(
            observed[0],
            InputEvent::Key(KeyEvent {
                code: KeyCode::Char('Z'),
                ..
            })
        ),
        "second listener must see the first listener's rewrite",
    );

    // The component also sees the rewritten event.
    let log = events.borrow();
    assert_eq!(log.len(), 1);
    assert!(matches!(
        log[0],
        InputEvent::Key(KeyEvent {
            code: KeyCode::Char('Z'),
            ..
        })
    ));
}
