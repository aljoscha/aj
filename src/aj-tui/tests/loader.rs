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
use aj_tui::tui::RenderHandle;

use support::strip_ansi;

// ---------------------------------------------------------------------------
// Default indicator
// ---------------------------------------------------------------------------

#[test]
fn default_indicator_renders_a_braille_spinner_frame() {
    let mut loader = Loader::with_identity_styles(RenderHandle::detached(), "loading");

    let rendered = loader.render(40);
    // Empty line, then `" " + spinner + " " + msg + trailing pad to
    // width` (pi-tui Loader extends Text("", padding_x=1, padding_y=0)
    // — the embedded Text emits left/right margins and pads to the
    // full render width).
    assert_eq!(rendered.len(), 2);
    assert_eq!(rendered[0], "");
    let line = strip_ansi(&rendered[1]);
    // First braille frame is '⠋' (see DEFAULT_FRAMES in loader.rs).
    // Leading space comes from Text's `padding_x = 1`.
    assert!(
        line.starts_with(" ⠋ "),
        "expected line to start with one-space pad + braille spinner + sep, got {:?}",
        line,
    );
    assert!(
        line.trim_end().ends_with("loading"),
        "expected message at end of (trimmed) line, got {:?}",
        line,
    );
}

#[test]
fn default_indicator_cycles_through_frames_over_time() {
    let loader = Loader::with_identity_styles(RenderHandle::detached(), "loading");

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
    let mut loader = Loader::with_identity_styles(RenderHandle::detached(), "working");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["A", "B", "C"])));

    let rendered = loader.render(20);
    let line = strip_ansi(&rendered[1]);
    // One of the custom frames — exact frame depends on timing, but
    // it must be A / B / C, never a braille glyph. Leading space comes
    // from Text's `padding_x = 1`.
    let trimmed = line.trim_start_matches(' ');
    assert!(
        trimmed.starts_with('A') || trimmed.starts_with('B') || trimmed.starts_with('C'),
        "expected custom frame at start of {:?}",
        line,
    );
    assert!(line.trim_end().ends_with("working"));
}

#[test]
fn custom_interval_drives_the_cycle_rate() {
    let mut loader = Loader::with_identity_styles(RenderHandle::detached(), "working");
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
    let mut loader = Loader::with_identity_styles(RenderHandle::detached(), "x");
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
    let mut loader = Loader::with_identity_styles(RenderHandle::detached(), "still");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["*"])));

    let first = loader.current_frame().to_string();
    thread::sleep(Duration::from_millis(50));
    let second = loader.current_frame().to_string();
    let third_rendered = strip_ansi(&loader.render(20)[1]);

    assert_eq!(first, "*");
    assert_eq!(first, second, "single-frame indicator must never advance");
    // Leading space comes from Text's `padding_x = 1`; trailing pad to
    // width 20 from the same.
    assert!(third_rendered.starts_with(" * "));
    assert!(third_rendered.trim_end().ends_with("still"));
}

// ---------------------------------------------------------------------------
// Empty frames (hidden indicator)
// ---------------------------------------------------------------------------

#[test]
fn empty_frames_hides_the_indicator_but_keeps_the_message() {
    let mut loader = Loader::with_identity_styles(RenderHandle::detached(), "message only");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(Vec::<&str>::new()),
    ));

    let rendered = loader.render(40);
    assert_eq!(rendered.len(), 2);
    assert_eq!(rendered[0], "");
    let line = strip_ansi(&rendered[1]);
    // No leading glyph, no separator space — just the message,
    // wrapped in Text's `padding_x = 1` margins (so one leading and
    // one trailing space, then trailing pad to render width).
    assert_eq!(line.trim(), "message only");
    assert!(
        line.starts_with(" message only "),
        "expected one-space pad before and after the message; got {line:?}",
    );
}

// ---------------------------------------------------------------------------
// Pi-tui Text-derived layout (F25)
//
// The Loader composes its body through an embedded Text component
// configured to match pi-tui's `extends Text("", paddingX=1, paddingY=0)`,
// then prepends a leading blank row. So:
//   - the spinner glyph sits one column in from the left (padding_x = 1),
//   - each visible row is right-padded to the full terminal width,
//   - long messages wrap at `width - 2` instead of overflowing.
// These tests pin the specific behaviors F25 fixed.
// ---------------------------------------------------------------------------

#[test]
fn loader_indents_spinner_one_space_to_match_text_padding_x_1() {
    // Pi-tui's Loader is `extends Text("", 1, 0)`, so the Text
    // component's `padding_x = 1` puts a single leading space before
    // the spinner glyph. A regression that reverted to
    // `format!("{} {}", frame, msg)` directly would lose this space.
    let mut loader = Loader::with_identity_styles(RenderHandle::detached(), "hi");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["@"])));

    let rendered = loader.render(20);
    let line = strip_ansi(&rendered[1]);
    assert!(
        line.starts_with(" @ hi"),
        "expected leading padding-x space before spinner; got {line:?}",
    );
}

#[test]
fn loader_pads_each_row_to_the_full_render_width() {
    // Text right-pads every content row to the full render width
    // (`leftMargin + line + rightMargin + spaces`). The previous shape
    // emitted `frame + " " + msg` with no trailing pad, leaving the
    // right side of the row at whatever the terminal had previously.
    let mut loader = Loader::with_identity_styles(RenderHandle::detached(), "hi");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["*"])));

    let rendered = loader.render(20);
    let line = strip_ansi(&rendered[1]);
    assert_eq!(
        line.chars().count(),
        20,
        "spinner row must span the full render width; got {line:?}",
    );
}

#[test]
fn long_message_wraps_to_render_width_minus_padding_x_pair() {
    // Pi-tui's Text wraps to `max(1, width - 2 * padding_x)`, so a
    // message that doesn't fit on one row spills to a second row
    // through the embedded Text. The old port ignored `width` and let
    // the message overflow.
    let mut loader = Loader::with_identity_styles(
        RenderHandle::detached(),
        "one two three four five six seven eight",
    );
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["*"])));

    let rendered = loader.render(20);
    // Leading blank + at least two rows of body content.
    assert!(
        rendered.len() >= 3,
        "expected the long message to wrap onto multiple rows; got {} row(s)",
        rendered.len(),
    );
    // Every body row stays within the render width.
    for line in &rendered[1..] {
        let visible = strip_ansi(line);
        assert_eq!(
            visible.chars().count(),
            20,
            "every body row must be padded to the render width; got {visible:?}",
        );
    }
}

// ---------------------------------------------------------------------------
// Verbatim indicator (already-styled frames bypass spinner_style)
// ---------------------------------------------------------------------------

#[test]
fn verbatim_indicator_skips_spinner_style_so_pre_styled_frames_arent_double_wrapped() {
    // Setup: a Loader with a `spinner_style` that wraps its argument in
    // a sentinel SGR sequence (here, the bold pair). The custom frames
    // already carry their own ANSI styling (red, in this fixture). With
    // `verbatim = true`, the loader must emit those frames as-is —
    // spinner_style stays out of the output entirely. Without verbatim,
    // the bold wrapper would land outside the red frame and the
    // assertion would fail.
    let mut loader = Loader::new(
        RenderHandle::detached(),
        Box::new(|s| format!("\x1b[1m{s}\x1b[22m")),
        Box::new(|s| s.to_string()),
        "loading",
    );

    let red_frame = "\x1b[31m*\x1b[0m";
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames([red_frame]).with_verbatim(true),
    ));

    let rendered = loader.render(20);
    let line = &rendered[1];
    // Frame must appear unchanged after the one-space `padding_x` pad,
    // with no bold wrapper anywhere.
    let expected_prefix = format!(" {red_frame}");
    assert!(
        line.starts_with(&expected_prefix),
        "verbatim frame must be emitted as-is right after the padding-x pad; got {line:?}",
    );
    assert!(
        !line.contains("\x1b[1m"),
        "verbatim must skip spinner_style; got bold wrapper in {line:?}",
    );

    // Sanity: stripped + trimmed, the line still reads as `* loading`.
    let stripped = strip_ansi(line);
    assert_eq!(stripped.trim(), "* loading");
}

#[test]
fn non_verbatim_indicator_still_applies_spinner_style() {
    // Counterpart to the verbatim test: confirm that without the flag,
    // spinner_style does wrap the frame. Guards against accidentally
    // making verbatim the default.
    let mut loader = Loader::new(
        RenderHandle::detached(),
        Box::new(|s| format!("\x1b[1m{s}\x1b[22m")),
        Box::new(|s| s.to_string()),
        "loading",
    );
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["*"])));

    let rendered = loader.render(20);
    let line = &rendered[1];
    assert!(
        line.contains("\x1b[1m*\x1b[22m"),
        "non-verbatim frame should be wrapped in spinner_style; got {line:?}",
    );
}

#[test]
fn set_indicator_none_clears_the_verbatim_flag() {
    // After the user goes verbatim and then resets to the default
    // braille spinner, spinner_style must apply again — the default
    // glyphs are plain text and the user's styling should reach them.
    let mut loader = Loader::new(
        RenderHandle::detached(),
        Box::new(|s| format!("\x1b[1m{s}\x1b[22m")),
        Box::new(|s| s.to_string()),
        "loading",
    );
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(["X"]).with_verbatim(true),
    ));
    let verbatim_line = loader.render(20)[1].clone();
    assert!(
        !verbatim_line.contains("\x1b[1m"),
        "sanity: verbatim should not include spinner_style wrapper",
    );

    loader.set_indicator(None);
    let default_line = loader.render(20)[1].clone();
    assert!(
        default_line.contains("\x1b[1m"),
        "after reset, spinner_style must wrap the default spinner glyph; got {default_line:?}",
    );
}

// ---------------------------------------------------------------------------
// Restoring the default
// ---------------------------------------------------------------------------

#[test]
fn set_indicator_none_restores_the_default_spinner() {
    let mut loader = Loader::with_identity_styles(RenderHandle::detached(), "x");
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
    let mut loader = Loader::with_identity_styles(tui.handle(), "loading");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(["A", "B", "C"]).with_interval(interval),
    ));
    tui.add_child(Box::new(loader));

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
    // start counting. Pattern: render_now advances time only
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

    let mut loader = Loader::with_identity_styles(tui.handle(), "static");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(["X"]).with_interval(interval),
    ));
    tui.add_child(Box::new(loader));

    // F39: construction + set_indicator each request a synchronous
    // render so a static loader paints immediately. Pull the
    // (coalesced) Render off the queue so the assertion below can
    // verify the *animation pump* doesn't fire any further renders.
    let _ = tokio::time::timeout(Duration::from_millis(64), tui.next_event()).await;

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

    let mut loader = Loader::with_identity_styles(tui.handle(), "loading");
    loader.set_indicator(Some(
        LoaderIndicatorOptions::with_frames(["A", "B", "C"]).with_interval(interval),
    ));
    tui.add_child(Box::new(loader));

    // Let the pump fire at least once, then access the loader through
    // the root to call stop(). We need to downcast through Container.
    // Pull the coalesced construction + pump Render off the queue
    // (F39: construction itself also requests a render now). The
    // longer 32ms timeout is needed because `support::async_tui::drain_ready`
    // uses a 0ms timeout that never lets the throttle window elapse
    // under paused time.
    support::async_tui::advance(interval * 2).await;
    let _ = tokio::time::timeout(Duration::from_millis(32), tui.next_event()).await;

    // Reach into the root child and stop the loader.
    if let Some(child) = tui.get_mut(0) {
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
    let _ = tokio::time::timeout(Duration::from_millis(32), tui.next_event()).await;

    // Now a long advance should produce no further events.
    support::async_tui::advance(interval * 10).await;
    let maybe = tokio::time::timeout(Duration::from_millis(1), tui.next_event()).await;
    assert!(
        maybe.is_err(),
        "stopped loader should not generate further render pings",
    );
}

// ---------------------------------------------------------------------------
// F39: synchronous render-request points
//
// State changes that affect the visible loader (`set_message`,
// `set_indicator`, construction itself) must request a render
// synchronously, so a static or empty-frame loader on an idle Tui
// surfaces the change without waiting for an unrelated render
// trigger. Mirrors pi-tui's `updateDisplay → requestRender` chain.
// ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn set_message_on_a_static_loader_requests_a_render() {
    // Setup: Tui with a Loader child whose indicator is a single
    // static frame, so no animation pump is running. Drain everything
    // pending (construction + set_indicator both request a render
    // under F39), then mutate the message and assert that the
    // mutation produces a fresh Render event.
    use aj_tui::components::loader::{Loader, LoaderIndicatorOptions};
    use aj_tui::tui::{Tui, TuiEvent};
    use std::time::Duration;

    let throttle = Duration::from_millis(16);
    let terminal = support::VirtualTerminal::new(20, 3);
    let mut tui = Tui::new(Box::new(terminal));
    tui.set_initial_render(false);
    tui.start().unwrap();

    let mut loader = Loader::with_identity_styles(tui.handle(), "loading");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["X"])));
    tui.add_child(Box::new(loader));

    // Drain the construction + set_indicator requests so the test's
    // post-set_message assertion isn't satisfied by an earlier event.
    // (Use a real timeout so the throttle window can elapse; a 0ms
    // timeout under paused tokio time fires before the throttle
    // ticks and ends up draining nothing.)
    let _ = tokio::time::timeout(Duration::from_millis(64), tui.next_event()).await;

    // Mutate the message through the root child. Reaching in this way
    // mirrors how the test below does `loader.stop()` — components
    // generally live inside a Container after `add_child`.
    let child = tui.get_mut(0).expect("expected at least one root child");
    let loader = child
        .as_any_mut()
        .downcast_mut::<Loader>()
        .expect("expected child to downcast to Loader");
    loader.set_message("done");

    // Advance past the throttle window so the request can drain.
    support::async_tui::advance(throttle * 2).await;

    let ev = tokio::time::timeout(throttle * 4, tui.next_event())
        .await
        .expect("set_message on a static loader should request a render")
        .expect("event loop should not have shut down");
    assert!(
        matches!(ev, TuiEvent::Render),
        "expected Render from set_message; got {ev:?}",
    );
}

#[tokio::test(start_paused = true)]
async fn set_indicator_swap_requests_a_render_even_when_new_indicator_is_static() {
    // Same shape as the set_message test but the trigger is a swap
    // to a different static indicator. Without F39, the swap would
    // sit unrendered (no animation pump runs for a static frame, no
    // input is arriving, and `set_initial_render(false)` keeps the
    // implicit bootstrap render off). With F39, set_indicator's
    // tail calls request_repaint and the swap surfaces immediately.
    use aj_tui::components::loader::{Loader, LoaderIndicatorOptions};
    use aj_tui::tui::{Tui, TuiEvent};
    use std::time::Duration;

    let throttle = Duration::from_millis(16);
    let terminal = support::VirtualTerminal::new(20, 3);
    let mut tui = Tui::new(Box::new(terminal));
    tui.set_initial_render(false);
    tui.start().unwrap();

    let mut loader = Loader::with_identity_styles(tui.handle(), "loading");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["A"])));
    tui.add_child(Box::new(loader));
    let _ = tokio::time::timeout(Duration::from_millis(64), tui.next_event()).await;

    let child = tui.get_mut(0).expect("expected at least one root child");
    let loader = child
        .as_any_mut()
        .downcast_mut::<Loader>()
        .expect("expected child to downcast to Loader");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["B"])));

    support::async_tui::advance(throttle * 2).await;

    let ev = tokio::time::timeout(throttle * 4, tui.next_event())
        .await
        .expect("set_indicator on a static loader should request a render")
        .expect("event loop should not have shut down");
    assert!(
        matches!(ev, TuiEvent::Render),
        "expected Render from set_indicator; got {ev:?}",
    );
}

#[tokio::test(start_paused = true)]
async fn freshly_constructed_loader_requests_a_render_with_initial_render_disabled() {
    // Setup: a Tui with `set_initial_render(false)` (so the implicit
    // bootstrap render is off) and otherwise idle. Constructing a
    // Loader against that Tui's handle should immediately request a
    // render via the constructor's `request_repaint` tail-call —
    // pi-tui's `setIndicator` runs at the tail of the constructor
    // and chains to `updateDisplay → requestRender`.
    //
    // Without F39, an idle Tui with `initial_render = false` and a
    // freshly-constructed multi-frame loader appears one `interval`
    // late (first pump tick), and a single-frame / empty-frame loader
    // doesn't appear at all until a separate render trigger fires.
    use aj_tui::components::loader::{Loader, LoaderIndicatorOptions};
    use aj_tui::tui::{Tui, TuiEvent};
    use std::time::Duration;

    let throttle = Duration::from_millis(16);
    let terminal = support::VirtualTerminal::new(20, 3);
    let mut tui = Tui::new(Box::new(terminal));
    tui.set_initial_render(false);
    tui.start().unwrap();

    // Use a single-frame indicator so no animation pump runs — the
    // only path to a render is the constructor's request_repaint.
    let mut loader = Loader::with_identity_styles(tui.handle(), "static");
    loader.set_indicator(Some(LoaderIndicatorOptions::with_frames(["X"])));
    tui.add_child(Box::new(loader));

    support::async_tui::advance(throttle * 2).await;

    let ev = tokio::time::timeout(throttle * 4, tui.next_event())
        .await
        .expect("Loader::new should request a render")
        .expect("event loop should not have shut down");
    assert!(
        matches!(ev, TuiEvent::Render),
        "expected Render from Loader::new; got {ev:?}",
    );
}
