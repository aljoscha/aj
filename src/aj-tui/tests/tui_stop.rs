//! Tests for `Tui::stop` idempotence and the `Drop` safety net.
//!
//! `Tui::stop` is the documented "clean exit" path. Because applications
//! routinely call it inside event loops that can early-return on a `?`,
//! the `Tui` also has a `Drop` impl that calls `stop()` as a fallback.
//! Both paths have to play well together:
//!
//! - `stop()` is idempotent — calling it twice must not emit the
//!   terminal-restore writes twice.
//! - The `Drop` impl runs unconditionally, but relies on `stop()`'s
//!   idempotence to stay a no-op when the caller already stopped.
//! - `Drop` also runs when the caller never called `stop()` at all
//!   (the safety-net case) and must still produce a valid
//!   terminal-restore sequence.

mod support;

use aj_tui::tui::Tui;

use support::{StaticLines, VirtualTerminal, render_now};

/// The bytes we expect `Tui::stop` to append to the terminal on the
/// single call: a line-ending and a cursor-show sequence. Anything
/// beyond this should also appear only once.
const STOP_TAIL: &str = "\r\n\x1b[?25h";

#[test]
fn stop_emits_terminal_restore_once() {
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.add_child(Box::new(StaticLines::new(["hello"])));
    render_now(&mut tui);

    terminal.clear_writes();
    tui.stop();

    let after_first = terminal.writes_joined();
    assert!(
        after_first.contains(STOP_TAIL),
        "first stop() should emit the restore tail; got {:?}",
        after_first,
    );
}

#[test]
fn stop_is_idempotent() {
    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.add_child(Box::new(StaticLines::new(["hello"])));
    render_now(&mut tui);

    terminal.clear_writes();
    tui.stop();
    let after_first = terminal.writes_joined();

    tui.stop();
    let after_second = terminal.writes_joined();

    assert_eq!(
        after_first, after_second,
        "second stop() must be a no-op; first wrote {:?}, second wrote {:?}",
        after_first, after_second,
    );
}

#[test]
fn drop_after_explicit_stop_does_not_double_restore() {
    // The common pattern: app calls `tui.stop()` on clean exit, then
    // the `Tui` drops at the end of the scope. The Drop impl calls
    // stop() again, but idempotence should keep it a no-op so we don't
    // emit a second "\r\n" or another cursor-show.
    let terminal = VirtualTerminal::new(20, 4);
    {
        let mut tui = Tui::new(Box::new(terminal.clone()));
        tui.add_child(Box::new(StaticLines::new(["hi"])));
        render_now(&mut tui);

        terminal.clear_writes();
        tui.stop();
    } // Tui dropped here; Drop::drop should be a no-op.

    let writes = terminal.writes_joined();
    // The restore tail must appear exactly once across stop() + Drop.
    let count = writes.matches(STOP_TAIL).count();
    assert_eq!(
        count, 1,
        "expected exactly one restore tail, got {} in writes {:?}",
        count, writes,
    );
}

#[test]
fn drop_restores_terminal_when_stop_was_not_called() {
    // The safety-net path: application forgot (or crashed) before
    // calling stop(). Drop must still emit the restore sequence so the
    // real terminal goes back to a sane state.
    let terminal = VirtualTerminal::new(20, 4);
    {
        let mut tui = Tui::new(Box::new(terminal.clone()));
        tui.add_child(Box::new(StaticLines::new(["hi"])));
        render_now(&mut tui);
        terminal.clear_writes();
        // No tui.stop() — let Drop run.
    }

    let writes = terminal.writes_joined();
    assert!(
        writes.contains(STOP_TAIL),
        "Drop should have emitted the restore tail; got {:?}",
        writes,
    );
}
