//! Shared test support for `aj-tui` integration tests.
//!
//! This module is included by every integration test via `mod support;`. It is
//! deliberately not a library: it lives under `tests/support/` so it is only
//! compiled in test builds and can reach for `dev-dependencies` (`vt100-ctt`,
//! `serial_test`) without polluting the public crate surface.
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
//! See `tests/support/README.md` for the broader testing philosophy.

#![allow(dead_code, unused_imports)]

#[path = "support/ansi.rs"]
pub mod ansi;
#[path = "support/async_tui.rs"]
pub mod async_tui;
#[path = "support/env.rs"]
pub mod env;
#[path = "support/fixtures.rs"]
pub mod fixtures;
#[path = "support/logging_terminal.rs"]
pub mod logging_terminal;
#[path = "support/themes.rs"]
pub mod themes;
#[path = "support/virtual_terminal.rs"]
pub mod virtual_terminal;

pub use ansi::{plain_lines, plain_lines_trim_end, strip_ansi, visible_index_of};
pub use env::{EnvGuard, with_env};
pub use fixtures::{InputRecorder, MutableLines, StaticLines, StaticOverlay};
pub use logging_terminal::LoggingVirtualTerminal;
pub use virtual_terminal::VirtualTerminal;

use aj_tui::keys::InputEvent;
use aj_tui::tui::Tui;

/// Drive a render pass to completion and synchronize the virtual terminal.
///
/// The synchronous engine renders immediately, so this helper is simply
/// `tui.render()`. It exists as a forward-compatibility seam: the async
/// tier (see [`async_tui`]) has its own timing model, and tests that
/// graduate onto it can swap this helper for one that awaits the
/// throttle without having to change shape.
pub fn wait_for_render(tui: &mut Tui) {
    tui.render();
}

/// Convenience for the idiomatic `tui.request_render(); wait_for_render(tui)`
/// sequence that dominates component-mutation tests.
///
/// The sync engine doesn't gate `render()` on `request_render()` (see the
/// "Intentional simplifications" section of `tests/support/README.md`), so
/// the `request_render()` call is purely about intent. Using this helper
/// keeps that intent visible while cutting the two-line mutation-then-
/// render ceremony down to one.
pub fn request_and_wait_for_render(tui: &mut Tui) {
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
/// No render is triggered here; call `wait_for_render(&mut tui)` afterwards
/// if the test asserts on rendered output.
pub fn send_keys<I>(tui: &mut Tui, events: I)
where
    I: IntoIterator<Item = InputEvent>,
{
    for event in events {
        tui.handle_input(&event);
    }
}
