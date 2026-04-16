//! Tests for the `Loader` component's indicator shapes.
//!
//! Covers the three shapes the `set_indicator` contract
//! offers: default (the built-in braille spinner), custom frames +
//! interval (animated cycle), single-frame (static), and empty-frame
//! (hidden indicator — message-only render).
//!
//! Timing-sensitive assertions use `std::thread::sleep` with short
//! intervals. They're tight enough that the tests run quickly and
//! loose enough to survive CI jitter.

mod support;

use std::thread;
use std::time::Duration;

use aj_tui::component::Component;
use aj_tui::components::loader::{Loader, LoaderIndicatorOptions};

use support::strip_ansi;

// ---------------------------------------------------------------------------
// Default indicator
// ---------------------------------------------------------------------------

#[test]
fn default_indicator_renders_a_braille_spinner_frame() {
    let mut loader = Loader::new("loading");

    let rendered = loader.render(40);
    // Empty line, then spinner + message.
    assert_eq!(rendered.len(), 2);
    assert_eq!(rendered[0], "");
    let line = strip_ansi(&rendered[1]);
    // First braille frame is '⠋' (see DEFAULT_FRAMES in loader.rs).
    assert!(
        line.starts_with('⠋'),
        "expected line to start with braille spinner, got {:?}",
        line,
    );
    assert!(line.ends_with("loading"));
}

#[test]
fn default_indicator_cycles_through_frames_over_time() {
    let loader = Loader::new("loading");

    let first = loader.current_frame().to_string();
    // Wait enough time to cross at least one frame boundary
    // (default interval = 80ms).
    thread::sleep(Duration::from_millis(100));
    let second = loader.current_frame().to_string();

    assert_ne!(
        first, second,
        "animated indicator should advance to a new frame after the interval",
    );
}

// ---------------------------------------------------------------------------
// Custom frames + interval
// ---------------------------------------------------------------------------

#[test]
fn custom_frames_drive_the_indicator() {
    let mut loader = Loader::new("working");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["A", "B", "C"])));

    let rendered = loader.render(20);
    let line = strip_ansi(&rendered[1]);
    // One of the custom frames — exact frame depends on timing, but
    // it must be A / B / C, never a braille glyph.
    assert!(
        line.starts_with('A') || line.starts_with('B') || line.starts_with('C'),
        "expected custom frame at start of {:?}",
        line,
    );
    assert!(line.ends_with("working"));
}

#[test]
fn custom_interval_drives_the_cycle_rate() {
    let mut loader = Loader::new("working");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(["A", "B"]).with_interval(Duration::from_millis(20)),
    ));

    let first = loader.current_frame().to_string();
    thread::sleep(Duration::from_millis(30));
    let second = loader.current_frame().to_string();

    assert_ne!(
        first, second,
        "with a 20ms interval the frame should advance within 30ms",
    );
}

#[test]
fn zero_interval_falls_back_to_the_default_interval() {
    // Defensive guard against `interval == 0`: keep the default
    // instead of ticking every frame. A zero-interval cycle would
    // spin the terminal with no useful visual effect.
    let mut loader = Loader::new("x");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(["A", "B"]).with_interval(Duration::ZERO),
    ));

    // Advance less than the default interval (80ms); the frame must
    // not have changed, since a non-zero default interval was used.
    let first = loader.current_frame().to_string();
    thread::sleep(Duration::from_millis(20));
    let second = loader.current_frame().to_string();
    assert_eq!(
        first, second,
        "zero interval should fall back to default (80ms), not tick every frame",
    );
}

// ---------------------------------------------------------------------------
// Single-frame (static)
// ---------------------------------------------------------------------------

#[test]
fn single_frame_indicator_renders_statically() {
    let mut loader = Loader::new("still");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["*"])));

    let first = loader.current_frame().to_string();
    thread::sleep(Duration::from_millis(50));
    let second = loader.current_frame().to_string();
    let third_rendered = strip_ansi(&loader.render(20)[1]);

    assert_eq!(first, "*");
    assert_eq!(first, second, "single-frame indicator must never advance");
    assert!(third_rendered.starts_with('*'));
    assert!(third_rendered.ends_with("still"));
}

// ---------------------------------------------------------------------------
// Empty frames (hidden indicator)
// ---------------------------------------------------------------------------

#[test]
fn empty_frames_hides_the_indicator_but_keeps_the_message() {
    let mut loader = Loader::new("message only");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(Vec::<&str>::new()),
    ));

    let rendered = loader.render(40);
    assert_eq!(rendered.len(), 2);
    assert_eq!(rendered[0], "");
    let line = strip_ansi(&rendered[1]);
    // No leading glyph, no separator space: just the message.
    assert_eq!(line, "message only");
}

// ---------------------------------------------------------------------------
// Restoring the default
// ---------------------------------------------------------------------------

#[test]
fn set_indicator_none_restores_the_default_spinner() {
    let mut loader = Loader::new("x");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["A"])));
    assert_eq!(loader.current_frame(), "A");

    loader.set_indicator(None);
    // Default first frame is the braille glyph.
    assert_eq!(loader.current_frame(), "⠋");
}

// ---------------------------------------------------------------------------
// Animation pump (auto-tick into the render loop)
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn loader_in_a_tui_tree_pings_the_render_loop_each_interval() {
    // The Loader's spinner frame is wall-clock-derived: render() picks
    // a frame from elapsed time. That only updates the screen if a
    // render actually fires — so the loader must drive its own ticks
    // when it's the only thing animating in an otherwise idle TUI.
    //
    // Setup: a Tui with a Loader child, no other render sources. The
    // Loader's animation pump should ping `request_render` once per
    // interval, and the throttle should coalesce those into
    // `TuiEvent::Render` events that the test pulls off the queue.
    use aj_tui::components::loader::{Loader, LoaderIndicatorOptions};
    use aj_tui::tui::{Tui, TuiEvent};
    use std::time::Duration;

    let interval = Duration::from_millis(50);
    let throttle = Duration::from_millis(16);
    let terminal = support::VirtualTerminal::new(20, 3);
    let mut tui = Tui::new(Box::new(terminal));
    tui.set_initial_render(false);
    tui.start().unwrap();

    // Add a multi-frame Loader so the pump is active. Set a
    // explicitly-non-default interval so we can advance time
    // deterministically.
    let mut loader = Loader::new("loading");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(["A", "B", "C"]).with_interval(interval),
    ));
    tui.root.add_child(Box::new(loader));

    // Yield once so the spawned animation task gets a chance to run
    // its first poll and register its interval timer with the
    // paused-clock test runtime. Without this, advance() can race
    // ahead of the timer registration and the pump misses ticks.
    tokio::task::yield_now().await;

    // Advance one loader interval plus a throttle window so:
    //   - the pump's interval fires at least once,
    //   - the resulting render request lives in the channel,
    //   - the next_event throttle has had time to elapse.
    support::async_tui::advance(interval + throttle * 2).await;

    // The throttle is created lazily on first `next_event` call, so
    // we have to call next_event *before* the throttle window can
    // start counting. Pattern: wait_for_render advances time only
    // implicitly through `tick().await`, so we do it manually here.
    let ev = tokio::time::timeout(throttle * 4, tui.next_event())
        .await
        .expect("loader pump should have produced a render event")
        .expect("event loop should not have shut down");
    assert!(
        matches!(ev, TuiEvent::Render),
        "expected a Render event from the loader pump; got {ev:?}",
    );
}

#[tokio::test(start_paused = true)]
async fn loader_animation_pump_does_not_run_for_static_single_frame_indicator() {
    // A static indicator (frames.len() <= 1) doesn't animate. The
    // pump should skip the spawn entirely so an idle TUI with only
    // a static loader stays quiet.
    use aj_tui::components::loader::{Loader, LoaderIndicatorOptions};
    use aj_tui::tui::Tui;
    use std::time::Duration;

    let interval = Duration::from_millis(50);
    let terminal = support::VirtualTerminal::new(20, 3);
    let mut tui = Tui::new(Box::new(terminal));
    tui.set_initial_render(false);
    tui.start().unwrap();

    let mut loader = Loader::new("static");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(["X"]).with_interval(interval),
    ));
    tui.root.add_child(Box::new(loader));

    support::async_tui::advance(interval * 5).await;

    let maybe = tokio::time::timeout(Duration::from_millis(1), tui.next_event()).await;
    assert!(
        maybe.is_err(),
        "static-indicator loader should not produce any pump renders",
    );
}

#[tokio::test(start_paused = true)]
async fn loader_animation_pump_stops_when_loader_stop_is_called() {
    // After `loader.stop()`, the pump should cancel its task. An
    // idle TUI past stop should not produce further Render events
    // attributable to the loader.
    use aj_tui::components::loader::{Loader, LoaderIndicatorOptions};
    use aj_tui::tui::Tui;
    use std::time::Duration;

    let interval = Duration::from_millis(50);
    let terminal = support::VirtualTerminal::new(20, 3);
    let mut tui = Tui::new(Box::new(terminal));
    tui.set_initial_render(false);
    tui.start().unwrap();

    let mut loader = Loader::new("loading");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(["A", "B", "C"]).with_interval(interval),
    ));
    tui.root.add_child(Box::new(loader));

    // Let the pump fire at least once, then access the loader through
    // the root to call stop(). We need to downcast through Container.
    support::async_tui::advance(interval * 2).await;
    let _ = support::async_tui::drain_ready(&mut tui).await;

    // Reach into the root child and stop the loader.
    if let Some(child) = tui.root.get_mut(0) {
        if let Some(loader) = child.as_any_mut().downcast_mut::<Loader>() {
            loader.stop();
        } else {
            panic!("expected child to downcast to Loader");
        }
    } else {
        panic!("expected at least one root child");
    }

    // Past the stop, drain anything still in flight (e.g. a tick
    // that fired between drain_ready and stop).
    let _ = support::async_tui::drain_ready(&mut tui).await;

    // Now a long advance should produce no further events.
    support::async_tui::advance(interval * 10).await;
    let maybe = tokio::time::timeout(Duration::from_millis(1), tui.next_event()).await;
    assert!(
        maybe.is_err(),
        "stopped loader should not generate further render pings",
    );
}
