//! Steady-state render cost measurement, gated behind `--ignored`.
//!
//! Mirrors the production load that motivated the validator skip:
//! a chat-sized frame (hundreds of lines, mostly ASCII with some
//! styled rows), re-rendered many times with only a single row
//! changing per frame (the spinner). Run with:
//!
//!     cargo test -p aj-tui --release --test render_cost -- --ignored --nocapture
//!
//! `--release` matters here — debug-mode UTF-8/grapheme work is
//! ~3-5× slower than release and would mislead the read.
//!
//! The test prints per-render timings; assertions are conservative
//! enough that random CI jitter won't trip them. The point is the
//! printed numbers, not the pass/fail.

use aj_tui_testkit as support;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::tui::Tui;

use support::VirtualTerminal;

/// A component that emits a fixed list of lines on each render, swappable
/// via a shared handle so the test can change exactly one row between
/// renders to model a spinner.
#[derive(Clone)]
struct ScriptedLines {
    lines: Rc<RefCell<Vec<String>>>,
}

impl ScriptedLines {
    fn new(initial: Vec<String>) -> Self {
        Self {
            lines: Rc::new(RefCell::new(initial)),
        }
    }

    fn replace(&self, idx: usize, value: String) {
        self.lines.borrow_mut()[idx] = value;
    }
}

impl Component for ScriptedLines {
    aj_tui::impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        self.lines.borrow().clone()
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }
}

/// Build a frame that loosely resembles a long chat conversation: a mix
/// of ASCII-only rows (the common case) and rows with a smattering of
/// ANSI styling (assistant headers, tool labels). Width 199 matches a
/// typical wide terminal. The byte count is intentionally similar to
/// the live conversation that motivated the investigation (~400 KB).
fn build_fake_chat(num_lines: usize, width: usize) -> Vec<String> {
    let mut out = Vec::with_capacity(num_lines);
    let body = "x".repeat(width.saturating_sub(2));
    // The styled row's visible width = "● assistant " (12 cells) +
    // body length. Trim the body to keep the total within `width`.
    let styled_prefix_visible = 12;
    let styled_body = "x".repeat(width.saturating_sub(styled_prefix_visible));
    let styled = format!("\x1b[36m●\x1b[39m \x1b[1massistant\x1b[22m {styled_body}");
    for i in 0..num_lines {
        if i % 17 == 0 {
            out.push(styled.clone());
        } else {
            out.push(body.clone());
        }
    }
    out
}

#[test]
#[ignore = "perf measurement; run with --ignored --nocapture"]
fn steady_state_render_cost() {
    const NUM_LINES: usize = 4000;
    const WIDTH: u16 = 199;
    const HEIGHT: u16 = 48;
    const ITERATIONS: u32 = 200;

    let virt = VirtualTerminal::new(WIDTH, HEIGHT);
    let scripted = ScriptedLines::new(build_fake_chat(NUM_LINES, WIDTH.into()));
    let handle = scripted.clone();

    let mut tui = Tui::new(Box::new(virt));
    tui.add_child(Box::new(scripted));

    // Warm-up: first render does a full paint and primes any caches.
    let cold = Instant::now();
    tui.render();
    let cold_ms = cold.elapsed().as_secs_f64() * 1000.0;

    // Steady-state: change exactly one row per render (mimicking a
    // spinner advancing its glyph). The diff engine should emit only
    // that one row to the terminal, and — with the validator skip —
    // the width check should only run on that one row too.
    //
    // Place the spinner row near the *end* of the visible viewport so
    // the diff engine takes the same "bottom-row repaint" path the
    // real status slot takes. The viewport sits at the bottom of the
    // frame (rendered area > terminal height), so an in-viewport row
    // is one near `NUM_LINES - 1`.
    let mut total = std::time::Duration::ZERO;
    let mut max = std::time::Duration::ZERO;
    let spinner_frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let spinner_row = NUM_LINES - 1;
    for i in 0..ITERATIONS {
        let frame = spinner_frames[usize::try_from(i).unwrap() % spinner_frames.len()];
        handle.replace(spinner_row, format!("{frame} Working…"));
        let t = Instant::now();
        tui.render();
        let dt = t.elapsed();
        total += dt;
        if dt > max {
            max = dt;
        }
    }
    let avg_us = (total.as_secs_f64() * 1_000_000.0) / f64::from(ITERATIONS);
    let max_us = max.as_secs_f64() * 1_000_000.0;

    eprintln!(
        "[render_cost] frame={NUM_LINES}lines x {WIDTH}cols, \
         cold={cold_ms:.2}ms, steady-state avg={avg_us:.1}µs, max={max_us:.1}µs",
    );

    // Comparison: turn validation off entirely and re-measure. If the
    // averages match, the skip path is doing its job — the remaining
    // cost is everything-but-validation. If they diverge, we still
    // have headroom inside the skip path itself.
    tui.set_strict_line_widths(false);
    let mut total_off = std::time::Duration::ZERO;
    for i in 0..ITERATIONS {
        let frame = spinner_frames[usize::try_from(i).unwrap() % spinner_frames.len()];
        handle.replace(spinner_row, format!("{frame} no-validator…"));
        let t = Instant::now();
        tui.render();
        total_off += t.elapsed();
    }
    let avg_off_us = (total_off.as_secs_f64() * 1_000_000.0) / f64::from(ITERATIONS);
    eprintln!(
        "[render_cost] validator-off steady-state avg={avg_off_us:.1}µs \
         (gap vs strict={:.1}µs)",
        avg_us - avg_off_us,
    );

    // Sanity bound. With the validator fix the average steady-state
    // render should be well under 5ms even in debug mode at this
    // frame size — pre-fix it was tens of milliseconds. The bound
    // exists to catch a regression where caching/skip-logic breaks.
    assert!(
        avg_us < 50_000.0,
        "steady-state render averaged {avg_us:.1}µs/frame; the validator \
         skip or diff engine likely regressed (was tens of ms pre-fix)",
    );
}
