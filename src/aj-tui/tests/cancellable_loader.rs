//! Tests for the `CancellableLoader` component.
//!
//! The underlying spinner logic is covered by the `Loader` component
//! itself; this file targets the cancel-on-`tui.select.cancel` surface
//! that `CancellableLoader` adds on top. By default that keybinding
//! resolves to Escape and Ctrl+C.

mod support;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use aj_tui::component::Component;
use aj_tui::components::cancellable_loader::CancellableLoader;
use aj_tui::keys::Key;

#[test]
fn cancel_flag_starts_unset() {
    let loader = CancellableLoader::new("working");
    assert!(!loader.is_aborted());
    assert!(!loader.cancel_flag().load(Ordering::SeqCst));
}

#[test]
fn escape_trips_the_cancel_flag_and_marks_the_event_handled() {
    let mut loader = CancellableLoader::new("working");
    let flag = loader.cancel_flag();
    assert!(!flag.load(Ordering::SeqCst));

    let handled = loader.handle_input(&Key::escape());
    assert!(handled, "Escape should be reported as handled");
    assert!(flag.load(Ordering::SeqCst));
    assert!(loader.is_aborted());
}

#[test]
fn ctrl_c_trips_the_cancel_flag_via_tui_select_cancel() {
    // `tui.select.cancel` defaults to `["escape", "ctrl+c"]`, so the
    // loader picks up Ctrl+C alongside Escape now that dispatch goes
    // through the keybindings registry.
    let mut loader = CancellableLoader::new("working");
    let flag = loader.cancel_flag();
    assert!(!flag.load(Ordering::SeqCst));

    let handled = loader.handle_input(&Key::ctrl('c'));
    assert!(handled, "Ctrl+C should be reported as handled");
    assert!(flag.load(Ordering::SeqCst));
    assert!(loader.is_aborted());
}

#[test]
fn unrelated_input_does_not_trigger_abort() {
    let mut loader = CancellableLoader::new("working");
    let flag = loader.cancel_flag();

    for key in [Key::char('x'), Key::enter(), Key::up()] {
        assert!(!loader.handle_input(&key));
    }
    assert!(!flag.load(Ordering::SeqCst));
}

#[test]
fn on_abort_callback_fires_exactly_once() {
    let count = Rc::new(Cell::new(0u32));
    let callback_count = Rc::clone(&count);

    let mut loader = CancellableLoader::new("working");
    loader.set_on_abort(Box::new(move || {
        callback_count.set(callback_count.get() + 1);
    }));

    // First Escape trips the flag and fires the callback.
    loader.handle_input(&Key::escape());
    assert_eq!(count.get(), 1);

    // Subsequent Escapes are idempotent: the flag is already set, the
    // callback should not run again.
    loader.handle_input(&Key::escape());
    loader.handle_input(&Key::escape());
    assert_eq!(
        count.get(),
        1,
        "on_abort should fire only on the transition false→true",
    );
}

#[test]
fn abort_method_sets_flag_without_firing_callback() {
    // The callback runs only in response to user input so programmatic
    // teardown paths can abort without re-entering application code.
    let count = Rc::new(Cell::new(0u32));
    let callback_count = Rc::clone(&count);

    let mut loader = CancellableLoader::new("working");
    loader.set_on_abort(Box::new(move || {
        callback_count.set(callback_count.get() + 1);
    }));

    loader.abort();
    assert!(loader.is_aborted());
    assert_eq!(count.get(), 0, "abort() should not fire on_abort");

    // Subsequent Escape shouldn't fire it either — flag is already set.
    loader.handle_input(&Key::escape());
    assert_eq!(count.get(), 0);
}

#[test]
fn cancel_flag_is_shared_with_workers_that_hold_a_clone() {
    // cancel_flag returns an Arc<AtomicBool>; cloning that Arc is how
    // async workers observe the cancel request.
    let mut loader = CancellableLoader::new("working");
    let worker_flag = loader.cancel_flag();
    let another_flag = loader.cancel_flag();

    loader.handle_input(&Key::escape());

    assert!(worker_flag.load(Ordering::SeqCst));
    assert!(another_flag.load(Ordering::SeqCst));
    assert!(loader.is_aborted());
}

#[test]
fn renders_the_same_shape_as_the_wrapped_loader() {
    // Structural check: CancellableLoader should produce a blank line
    // + a spinner-with-message line just like Loader does. (We don't
    // assert on the spinner frame; any frame is fine.)
    let mut loader = CancellableLoader::new("testing");
    let lines = loader.render(40);
    assert_eq!(lines.len(), 2, "loader should produce two lines");
    assert_eq!(lines[0], "", "first line is the blank spacer");
    assert!(
        lines[1].contains("testing"),
        "second line should include the message; got {:?}",
        lines[1],
    );
}

#[test]
fn focused_state_roundtrips_through_set_focused() {
    let mut loader = CancellableLoader::new("working");
    assert!(!loader.is_focused());

    loader.set_focused(true);
    assert!(loader.is_focused());

    loader.set_focused(false);
    assert!(!loader.is_focused());
}
