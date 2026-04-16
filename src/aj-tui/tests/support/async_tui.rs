//! Async-tier support for tests that exercise [`aj_tui::tui::Tui`]'s
//! tokio-backed event loop directly. Intentionally separate from the
//! sync support helpers: only the small number of tests that
//! specifically care about throttle timing or input-stream plumbing
//! need to pull this in.
//!
//! Tests that use this module almost always want `#[tokio::test(start_paused
//! = true)]` so they can `tokio::time::advance` the throttle deterministically
//! rather than relying on real sleeps.

use std::time::Duration;

use aj_tui::keys::InputEvent;
use aj_tui::tui::{Tui, TuiEvent};
use tokio::sync::mpsc;

use super::virtual_terminal::VirtualTerminal;

/// Build a `Tui` wrapped around a fresh [`VirtualTerminal`] and return
/// the input sender alongside it so the test can push synthetic
/// [`InputEvent`]s into the TUI's event loop.
///
/// Starts the `Tui` (so `next_event` is ready to be awaited) and
/// disables the implicit initial render; tests that want the
/// bootstrap render should opt back in via `tui.set_initial_render(true)`.
pub fn channel_tui(columns: u16, rows: u16) -> (Tui, mpsc::UnboundedSender<InputEvent>) {
    let terminal = VirtualTerminal::new(columns, rows);
    let input_tx = terminal.input_sender();
    let mut tui = Tui::new(Box::new(terminal));
    tui.set_initial_render(false);
    tui.start().expect("start virtual terminal");
    (tui, input_tx)
}

/// Advance tokio's paused clock by `delta`. Convenience wrapper so tests read
/// as `advance(interval * 2).await` rather than pulling in `tokio::time`.
pub async fn advance(delta: Duration) {
    tokio::time::advance(delta).await;
}

/// Drain all immediately-available events from the `Tui` without
/// blocking, returning them in arrival order. Because `Tui::next_event`
/// is cancellation-safe, this is just a loop with a zero-duration
/// timeout.
pub async fn drain_ready(tui: &mut Tui) -> Vec<TuiEvent> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_millis(0), tui.next_event()).await {
            Ok(Some(ev)) => out.push(ev),
            _ => return out,
        }
    }
}

/// Pump `Tui::next_event` until the next throttled render fires,
/// dispatching any intervening input events to the `Tui`, then call
/// `tui.render()` and return.
///
/// Mirrors the sync `support::wait_for_render` helper so tests that
/// graduate from the sync engine onto the async loop keep the same
/// shape. Returns early without rendering if the event loop ends
/// (e.g. after shutdown); in that case the test's own timeout or
/// assertion will surface the problem.
pub async fn wait_for_render(tui: &mut Tui) {
    loop {
        match tui.next_event().await {
            Some(TuiEvent::Input(event)) => tui.handle_input(&event),
            Some(TuiEvent::Render) => {
                tui.render();
                return;
            }
            None => return,
        }
    }
}
