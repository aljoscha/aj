//! Tests for the async `Tui` event loop.
//!
//! These tests exercise the tokio-backed `Tui::next_event` in isolation from
//! the synchronous render engine — the point is to prove the throttle
//! coalesces, input passes through, and clean shutdown works. Rendering
//! correctness is covered by the sync-tier tests in `tui_render.rs`; this
//! file is about the async glue.
//!
//! Every test runs with `tokio::test(start_paused = true)` so the throttle
//! interval is driven by `tokio::time::advance` rather than real wall-clock
//! time. That keeps the tests deterministic and fast.

mod support;

use std::time::Duration;

use aj_tui::keys::InputEvent;
use aj_tui::tui::{Tui, TuiEvent};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use support::async_tui::{advance, channel_tui, wait_for_render};
use support::{InputRecorder, StaticLines, VirtualTerminal};

const INTERVAL: Duration = Duration::from_millis(16);

fn char_event(c: char) -> InputEvent {
    InputEvent::Key(KeyEvent {
        code: KeyCode::Char(c),
        modifiers: KeyModifiers::NONE,
        kind: KeyEventKind::Press,
        state: crossterm::event::KeyEventState::NONE,
    })
}

fn as_char(event: &TuiEvent) -> Option<char> {
    match event {
        TuiEvent::Input(InputEvent::Key(k)) => match k.code {
            KeyCode::Char(c) => Some(c),
            _ => None,
        },
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Render coalescing
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn multiple_render_requests_within_window_collapse_into_one() {
    let (mut tui, _input_tx) = channel_tui(20, 3);
    let handle = tui.handle();

    // Blast N render requests before the throttle fires.
    for _ in 0..10 {
        handle.request_render();
    }

    // Advance just under the interval: no render yet.
    advance(INTERVAL - Duration::from_millis(1)).await;

    // Advance past the interval: exactly one Render event should be ready.
    advance(Duration::from_millis(2)).await;
    let ev = tui.next_event().await;
    assert!(matches!(ev, Some(TuiEvent::Render)));

    // No second render should be pending from the previous burst.
    advance(INTERVAL * 4).await;
    let maybe = tokio::time::timeout(Duration::from_millis(1), tui.next_event()).await;
    assert!(maybe.is_err(), "no second render should have fired");
}

#[tokio::test(start_paused = true)]
async fn requests_across_windows_produce_separate_renders() {
    let (mut tui, _input_tx) = channel_tui(20, 3);
    let handle = tui.handle();

    handle.request_render();
    advance(INTERVAL).await;
    let ev1 = tui.next_event().await;
    assert!(matches!(ev1, Some(TuiEvent::Render)));

    handle.request_render();
    advance(INTERVAL).await;
    let ev2 = tui.next_event().await;
    assert!(matches!(ev2, Some(TuiEvent::Render)));
}

#[tokio::test(start_paused = true)]
async fn no_render_event_without_a_request() {
    let (mut tui, _input_tx) = channel_tui(20, 3);

    // Nothing requested. Advance well past multiple throttle windows.
    advance(INTERVAL * 5).await;

    let maybe = tokio::time::timeout(Duration::from_millis(1), tui.next_event()).await;
    assert!(
        maybe.is_err(),
        "Tui should not yield a Render without a pending request",
    );
}

#[tokio::test(start_paused = true)]
async fn total_renders_counts_one_per_coalesced_burst() {
    // Regression guard for `Tui::total_renders()`. Ten render
    // requests inside a single throttle window should produce one
    // `TuiEvent::Render`; calling `tui.render()` on that event
    // increments `total_renders()` by exactly 1.
    //
    // The counter exists so async-coalescing tests can assert on
    // render count directly, instead of relying on throttle-timing
    // asserts that are prone to flaking on slow CI machines.
    let (mut tui, _input_tx) = channel_tui(20, 3);
    let handle = tui.handle();
    // Disable the implicit initial render so the counter starts
    // at the explicit-request baseline.
    tui.set_initial_render(false);

    let baseline = tui.total_renders();

    for _ in 0..10 {
        handle.request_render();
    }

    // Past the throttle window: exactly one Render event fires, and
    // the callee-supplied `tui.render()` increments the counter once.
    advance(INTERVAL + Duration::from_millis(1)).await;
    let ev = tui.next_event().await;
    assert!(matches!(ev, Some(TuiEvent::Render)));
    tui.render();
    assert_eq!(
        tui.total_renders() - baseline,
        1,
        "coalesced burst yields exactly one render",
    );

    // A second, independent burst in a later window increments the
    // counter by another 1.
    for _ in 0..5 {
        handle.request_render();
    }
    advance(INTERVAL + Duration::from_millis(1)).await;
    let ev = tui.next_event().await;
    assert!(matches!(ev, Some(TuiEvent::Render)));
    tui.render();
    assert_eq!(
        tui.total_renders() - baseline,
        2,
        "each throttle window counts as one render",
    );
}

// ---------------------------------------------------------------------------
// Initial render
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn initial_render_fires_without_an_explicit_request() {
    let terminal = VirtualTerminal::new(20, 3);
    let mut tui = Tui::new(Box::new(terminal));
    tui.start().unwrap();
    // `set_initial_render(true)` is the default; make it explicit so the
    // test's intent is obvious.
    tui.set_initial_render(true);

    // Advance one interval; the implicit initial request should fire.
    advance(INTERVAL).await;
    let ev = tui.next_event().await;
    assert!(matches!(ev, Some(TuiEvent::Render)));
}

// ---------------------------------------------------------------------------
// Input delivery
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn input_events_pass_through_immediately() {
    let (mut tui, input_tx) = channel_tui(20, 3);

    input_tx.send(char_event('x')).unwrap();

    let ev = tui.next_event().await;
    assert_eq!(as_char(&ev.expect("event")), Some('x'));
}

#[tokio::test(start_paused = true)]
async fn input_takes_priority_over_pending_render() {
    let (mut tui, input_tx) = channel_tui(20, 3);
    let handle = tui.handle();

    handle.request_render();
    input_tx.send(char_event('a')).unwrap();

    // Even though a render is pending, the `biased` select should
    // prefer the ready input event. Advance time only a little to ensure the
    // throttle hasn't fired yet.
    advance(Duration::from_millis(1)).await;
    let ev = tui.next_event().await;
    assert_eq!(as_char(&ev.expect("event")), Some('a'));
}

#[tokio::test(start_paused = true)]
async fn input_and_render_interleave_correctly() {
    let (mut tui, input_tx) = channel_tui(20, 3);
    let handle = tui.handle();

    input_tx.send(char_event('1')).unwrap();
    handle.request_render();
    input_tx.send(char_event('2')).unwrap();

    let ev1 = tui.next_event().await;
    assert_eq!(as_char(&ev1.clone().unwrap()), Some('1'));

    let ev2 = tui.next_event().await;
    assert_eq!(as_char(&ev2.clone().unwrap()), Some('2'));

    advance(INTERVAL).await;
    let ev3 = tui.next_event().await;
    assert!(matches!(ev3, Some(TuiEvent::Render)));
}

// ---------------------------------------------------------------------------
// Shutdown
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn event_loop_ends_cleanly_when_input_closes_and_no_handles_remain() {
    let (mut tui, input_tx) = channel_tui(20, 3);

    drop(input_tx);
    // Drop the tui's own render sender by dropping all handles except the
    // internal one. The internal sender stays alive via tui.render_tx, so
    // next_event continues to block on input. Drop the tui after the test.
    // To model "no more sources of work" we only drop input_tx here; see
    // the paired test below for the flush-pending case. Here we confirm
    // that with input closed and no pending render, next_event blocks
    // (doesn't spin).
    let maybe = tokio::time::timeout(Duration::from_millis(1), tui.next_event()).await;
    assert!(
        maybe.is_err(),
        "next_event should block once input closes with no pending render",
    );
}

#[tokio::test(start_paused = true)]
async fn pending_render_is_flushed_after_input_closes() {
    let (mut tui, input_tx) = channel_tui(20, 3);
    let handle = tui.handle();

    handle.request_render();
    drop(input_tx);

    advance(INTERVAL).await;
    let ev = tui.next_event().await;
    assert!(
        matches!(ev, Some(TuiEvent::Render)),
        "expected pending render to flush before/after input close",
    );
}

// ---------------------------------------------------------------------------
// RenderHandle properties
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn render_handle_is_send_clone_usable_from_spawned_tasks() {
    let (mut tui, _input_tx) = channel_tui(20, 3);
    let handle = tui.handle();

    let h = handle.clone();
    tokio::spawn(async move {
        for _ in 0..5 {
            h.request_render();
        }
    })
    .await
    .unwrap();

    advance(INTERVAL).await;
    let ev = tui.next_event().await;
    assert!(matches!(ev, Some(TuiEvent::Render)));
}

// ---------------------------------------------------------------------------
// wait_for_render helper
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn wait_for_render_renders_once_throttle_fires() {
    let terminal = VirtualTerminal::new(20, 3);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_initial_render(false);
    tui.start().unwrap();
    tui.add_child(Box::new(StaticLines::new(["hello"])));

    let handle = tui.handle();
    handle.request_render();
    advance(INTERVAL).await;

    wait_for_render(&mut tui).await;

    assert_eq!(terminal.viewport()[0], "hello");
}

#[tokio::test(start_paused = true)]
async fn wait_for_render_dispatches_pending_input_before_rendering() {
    let terminal = VirtualTerminal::new(20, 3);
    let input_tx = terminal.input_sender();
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_initial_render(false);
    tui.start().unwrap();
    let (recorder, events) = InputRecorder::new();
    tui.add_child(Box::new(recorder));
    tui.set_focus(Some(0));

    // Input first, then a render request. Both are ready before the
    // throttle fires.
    input_tx.send(char_event('a')).unwrap();
    let handle = tui.handle();
    handle.request_render();
    advance(INTERVAL).await;

    wait_for_render(&mut tui).await;

    let recorded = events.borrow();
    assert_eq!(recorded.len(), 1, "input should have been dispatched");
    assert!(matches!(
        recorded[0],
        InputEvent::Key(KeyEvent {
            code: KeyCode::Char('a'),
            ..
        }),
    ));
}

/// Mirror of `tests/support_smoke.rs::virtual_terminal_round_trips_a_render`
/// ported onto the async event loop. Shares the shape so the two tiers stay
/// signature-compatible: if the sync version passes and this one fails, the
/// regression is in the async glue, not in the engine.
#[tokio::test(start_paused = true)]
async fn async_tui_round_trips_a_render() {
    let terminal = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.set_initial_render(false);
    tui.start().unwrap();
    tui.add_child(Box::new(StaticLines::new(["hello", "world"])));

    let handle = tui.handle();
    handle.request_render();
    advance(INTERVAL).await;

    wait_for_render(&mut tui).await;

    let viewport = terminal.viewport();
    assert_eq!(viewport.len(), 10);
    assert_eq!(viewport[0], "hello");
    assert_eq!(viewport[1], "world");
    assert_eq!(viewport[2], "");
}
