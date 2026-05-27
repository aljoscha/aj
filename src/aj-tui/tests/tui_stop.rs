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

use aj_tui::capabilities::{
    ImageProtocol, TerminalCapabilities, reset_capabilities_cache, set_capabilities,
};
use aj_tui::tui::Tui;

use support::{StaticLines, VirtualTerminal, render_now};

/// The bytes we expect `Tui::stop` to append to the terminal on the
/// single call: a line-ending and a cursor-show sequence. Anything
/// beyond this should also appear only once.
const STOP_TAIL: &str = "\r\n\x1b[?25h";

/// Pin the process-global capabilities cache to a known
/// no-images value so `stop`'s Kitty-caps gate (which would
/// otherwise emit a bulk-delete escape between the trailing
/// `\r\n` and the `\x1b[?25h`) doesn't fragment the
/// [`STOP_TAIL`] substring these tests assert on. Paired with
/// `#[serial]` so the override doesn't race tests that
/// genuinely need different caps.
fn pin_no_image_caps() {
    set_capabilities(TerminalCapabilities {
        hyperlinks: false,
        true_color: false,
        images: None,
    });
}

#[test]
#[serial_test::serial]
fn stop_emits_terminal_restore_once() {
    pin_no_image_caps();
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
    reset_capabilities_cache();
}

#[test]
#[serial_test::serial]
fn stop_is_idempotent() {
    pin_no_image_caps();
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
    reset_capabilities_cache();
}

#[test]
#[serial_test::serial]
fn drop_after_explicit_stop_does_not_double_restore() {
    // The common pattern: app calls `tui.stop()` on clean exit, then
    // the `Tui` drops at the end of the scope. The Drop impl calls
    // stop() again, but idempotence should keep it a no-op so we don't
    // emit a second "\r\n" or another cursor-show.
    pin_no_image_caps();
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
    reset_capabilities_cache();
}

#[test]
#[serial_test::serial]
fn drop_restores_terminal_when_stop_was_not_called() {
    // The safety-net path: application forgot (or crashed) before
    // calling stop(). Drop must still emit the restore sequence so the
    // real terminal goes back to a sane state.
    pin_no_image_caps();
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
    reset_capabilities_cache();
}

#[test]
#[serial_test::serial]
fn stop_emits_bulk_kitty_delete_when_kitty_caps() {
    // When the host terminal supports Kitty graphics, `stop` must
    // emit a bulk delete-by-visibility before handing the terminal
    // back to the shell, so any lingering image placements don't
    // bleed into the post-exit scrollback. Gated on Kitty caps:
    // other terminals don't see the escape.
    set_capabilities(TerminalCapabilities {
        hyperlinks: false,
        true_color: false,
        images: Some(ImageProtocol::Kitty),
    });

    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.add_child(Box::new(StaticLines::new(["hello"])));
    render_now(&mut tui);

    terminal.clear_writes();
    tui.stop();

    let writes = terminal.writes_joined();
    assert!(
        writes.contains("\x1b_Ga=d,d=A"),
        "stop() under Kitty caps should emit the bulk delete; got {:?}",
        writes,
    );

    reset_capabilities_cache();
}

#[test]
#[serial_test::serial]
fn stop_omits_bulk_kitty_delete_without_kitty_caps() {
    // Non-Kitty terminals (or capability-unknown environments) must
    // not see the Kitty bulk-delete escape on shutdown — it would
    // be harmless on most terminals but emitting it gratuitously
    // muddies the wire.
    set_capabilities(TerminalCapabilities {
        hyperlinks: false,
        true_color: false,
        images: None,
    });

    let terminal = VirtualTerminal::new(20, 4);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.add_child(Box::new(StaticLines::new(["hello"])));
    render_now(&mut tui);

    terminal.clear_writes();
    tui.stop();

    let writes = terminal.writes_joined();
    assert!(
        !writes.contains("\x1b_Ga=d,d=A"),
        "stop() without Kitty caps must not emit the bulk delete; got {:?}",
        writes,
    );

    reset_capabilities_cache();
}
