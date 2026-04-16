//! Smoke test: verifies the `tests/support/` module compiles and the
//! `VirtualTerminal` can be plugged into a `Tui`. If this file fails to
//! build, every other integration test will fail for the same reason, so
//! keeping the smoke check minimal and explicit makes debugging easier.

mod support;

use aj_tui::tui::Tui;

use support::{StaticLines, VirtualTerminal, render_now, request_and_render_now};

#[test]
fn virtual_terminal_round_trips_a_render() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.add_child(Box::new(StaticLines::new(["hello", "world"])));

    render_now(&mut tui);

    let viewport = terminal.viewport();
    assert_eq!(viewport.len(), 10);
    assert_eq!(viewport[0], "hello");
    assert_eq!(viewport[1], "world");
    assert_eq!(viewport[2], "");
}

#[test]
fn request_and_render_now_drives_a_full_render_pass() {
    // After a state change, the single helper call should both mark the
    // render as requested (intent) and flush it to the terminal, so
    // tests can stay in a one-line-per-frame shape.
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.add_child(Box::new(StaticLines::new(["initial"])));

    request_and_render_now(&mut tui);
    assert_eq!(terminal.viewport()[0], "initial");
    assert!(
        !tui.is_render_requested(),
        "rendering clears the pending-request flag",
    );
}
