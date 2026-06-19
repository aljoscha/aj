//! Shared test support for `aj-tui` integration tests.
//!
//! This is a dev-dependency-only crate: it depends on `aj-tui` and is pulled
//! in by `aj-tui`'s own integration tests and by other crates' tests (e.g.
//! the `aj` binary's replay-parity test) that need to drive a real `Tui`
//! against a headless terminal. Living in its own crate keeps the harness out
//! of `aj-tui`'s shipped public API while still letting any test in the
//! workspace share one `VirtualTerminal`.
//!
//! Test files pull it in with `use aj_tui_testkit as support;`, so every
//! `support::*` reference in the suite resolves here.
//!
//! The submodules provide:
//!
//! - [`VirtualTerminal`]: headless terminal backed by a VT100 parser. Tests
//!   drive rendering through a `Tui` that writes into this terminal, then
//!   assert against its viewport, cursor, and per-cell attributes.
//! - [`LoggingVirtualTerminal`]: naming alias used when a test is
//!   specifically about the raw escape sequences being written.
//! - [`ansi`]: `strip_ansi` / `plain_lines` / `visible_index_of` helpers
//!   for assertions that don't care about SGR codes.
//! - [`env`]: RAII env-var guards for tests that must toggle process state.
//! - [`fixtures`]: reusable component fixtures (`StaticLines`,
//!   `InputRecorder`, etc.).
//! - [`themes`]: shared theme fixtures for themable components.
//!
//! See `README.md` for the broader testing philosophy.

pub mod ansi;
pub mod async_tui;
pub mod env;
pub mod fixtures;
pub mod logging_terminal;
pub mod themes;
pub mod virtual_terminal;

pub use ansi::{plain_lines, plain_lines_trim_end, strip_ansi, visible_index_of};
pub use env::{EnvGuard, with_env};
pub use fixtures::{InputRecorder, MutableLines, StaticLines, StaticOverlay};
pub use logging_terminal::LoggingVirtualTerminal;
pub use virtual_terminal::VirtualTerminal;

use aj_tui::keys::InputEvent;
use aj_tui::tui::Tui;

/// Drive a synchronous render pass and let the virtual terminal
/// observe its writes.
///
/// On the sync engine `tui.render()` is unconditional and immediate, so
/// this helper is just a thin `tui.render()` call — there is nothing to
/// wait for. The async-tier counterpart, [`async_tui::wait_for_render`],
/// drives a real async render pass and waits for the engine to settle.
///
/// The deliberate name asymmetry — `render_now()` vs
/// `wait_for_render().await` — makes the engine choice obvious at the
/// call site.
pub fn render_now(tui: &mut Tui) {
    tui.render();
}

/// Convenience for the idiomatic `tui.request_render(); render_now(tui)`
/// sequence that dominates component-mutation tests.
///
/// The sync engine doesn't gate `render()` on `request_render()` (see the
/// "Intentional simplifications" section of `README.md`), so the
/// `request_render()` call is purely about intent. Using this helper
/// keeps that intent visible while cutting the two-line mutation-then-
/// render ceremony down to one.
pub fn request_and_render_now(tui: &mut Tui) {
    tui.request_render();
    tui.render();
}

/// Dispatch a batch of `InputEvent`s to the `Tui` in order.
///
/// Shorthand for the common "press these three keys" test setup that
/// otherwise repeats `tui.handle_input(&event)` per event. The iterator
/// accepts anything yielding owned `InputEvent`s (`IntoIterator`), so both
/// literal arrays and pre-built vectors work:
///
/// ```ignore
/// use aj_tui::keys::Key;
/// support::send_keys(&mut tui, [Key::char('h'), Key::char('i'), Key::enter()]);
/// ```
///
/// No render is triggered here; call `render_now(&mut tui)` afterwards
/// if the test asserts on rendered output.
pub fn send_keys<I>(tui: &mut Tui, events: I)
where
    I: IntoIterator<Item = InputEvent>,
{
    for event in events {
        tui.handle_input(&event);
    }
}
