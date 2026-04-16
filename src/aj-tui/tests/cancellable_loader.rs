//! Tests for the `CancellableLoader` component.
//!
//! The underlying spinner logic is covered by the `Loader` component
//! itself; this file targets the cancel-on-`tui.select.cancel` surface
//! that `CancellableLoader` adds on top. By default that keybinding
//! resolves to Escape and Ctrl+C.

mod support;

use std::cell::Cell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::components::cancellable_loader::CancellableLoader;
use aj_tui::keys::Key;
use aj_tui::tui::RenderHandle;

#[test]
fn cancel_token_starts_unaborted() {
    let loader = CancellableLoader::with_identity_styles(RenderHandle::detached(), "working");
    assert!(!loader.is_aborted());
    assert!(!loader.cancel_token().is_cancelled());
}

#[test]
fn escape_trips_the_cancel_token_and_marks_the_event_handled() {
    let mut loader = CancellableLoader::with_identity_styles(RenderHandle::detached(), "working");
    let token = loader.cancel_token();
    assert!(!token.is_cancelled());

    let handled = loader.handle_input(&Key::escape());
    assert!(handled, "Escape should be reported as handled");
    assert!(token.is_cancelled());
    assert!(loader.is_aborted());
}

#[test]
fn ctrl_c_trips_the_cancel_token_via_tui_select_cancel() {
    // `tui.select.cancel` defaults to `["escape", "ctrl+c"]`, so the
    // loader picks up Ctrl+C alongside Escape now that dispatch goes
    // through the keybindings registry.
    let mut loader = CancellableLoader::with_identity_styles(RenderHandle::detached(), "working");
    let token = loader.cancel_token();
    assert!(!token.is_cancelled());

    let handled = loader.handle_input(&Key::ctrl('c'));
    assert!(handled, "Ctrl+C should be reported as handled");
    assert!(token.is_cancelled());
    assert!(loader.is_aborted());
}

#[test]
fn unrelated_input_does_not_trigger_abort() {
    let mut loader = CancellableLoader::with_identity_styles(RenderHandle::detached(), "working");
    let token = loader.cancel_token();

    for key in [Key::char('x'), Key::enter(), Key::up()] {
        assert!(!loader.handle_input(&key));
    }
    assert!(!token.is_cancelled());
}

#[test]
fn on_abort_callback_fires_on_every_cancel_press() {
    // Mirrors pi-tui's `handleInput`, which calls
    // `this.abortController.abort(); this.onAbort?.();` unconditionally
    // on every matching key press. Cancellation itself is idempotent
    // (the token only flips false→true once); the callback is not.
    let count = Rc::new(Cell::new(0u32));
    let callback_count = Rc::clone(&count);

    let mut loader = CancellableLoader::with_identity_styles(RenderHandle::detached(), "working");
    loader.set_on_abort(Box::new(move || {
        callback_count.set(callback_count.get() + 1);
    }));

    loader.handle_input(&Key::escape());
    assert_eq!(count.get(), 1);

    // Subsequent cancel-key presses still fire the callback (matches
    // pi); the underlying token stays cancelled either way.
    loader.handle_input(&Key::escape());
    loader.handle_input(&Key::escape());
    assert_eq!(count.get(), 3, "on_abort fires on every cancel-key press");
    assert!(loader.is_aborted());
}

#[test]
fn cancel_token_is_shared_with_workers_that_hold_a_clone() {
    // cancel_token returns a CancellationToken; cloning it is how
    // async workers observe the cancel request. Clones share state.
    let mut loader = CancellableLoader::with_identity_styles(RenderHandle::detached(), "working");
    let worker_token = loader.cancel_token();
    let another_token = loader.cancel_token();

    loader.handle_input(&Key::escape());

    assert!(worker_token.is_cancelled());
    assert!(another_token.is_cancelled());
    assert!(loader.is_aborted());
}

#[test]
fn renders_the_same_shape_as_the_wrapped_loader() {
    // Structural check: CancellableLoader should produce a blank line
    // + a spinner-with-message line just like Loader does. (We don't
    // assert on the spinner frame; any frame is fine.)
    let mut loader = CancellableLoader::with_identity_styles(RenderHandle::detached(), "testing");
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
    let mut loader = CancellableLoader::with_identity_styles(RenderHandle::detached(), "working");
    assert!(!loader.is_focused());

    loader.set_focused(true);
    assert!(loader.is_focused());

    loader.set_focused(false);
    assert!(!loader.is_focused());
}
