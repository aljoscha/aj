//! Render-time micro-benchmark for a long-scrollback chat.
//!
//! Not run by default — the assertion is observational. Run with:
//!
//! ```text
//! cargo test --release -p aj-tui --test perf_render -- --ignored --nocapture
//! ```
//!
//! The shape models the stuck-session scenario: a static scrollback of
//! ~25k lines, the same line count we saw in the diff engine when the
//! loader pump was pegging CPU. We measure the per-frame cost of
//! [`Tui::render`] in steady state (cache-hits, byte-identical
//! repaints) so the numbers reflect the work the loader-pump tick
//! actually performs: walk every child, allocate cached lines,
//! normalize / validate / diff against `previous_lines`.

use aj_tui_testkit as support;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::tui::Tui;

use support::VirtualTerminal;

/// Component that returns a fixed list of pre-computed lines.
/// `render` clones the buffer on every call — same shape as the cached
/// `text_box::render` path (return `cache.lines.clone()`).
struct ScriptedLines {
    lines: Rc<RefCell<Vec<String>>>,
}

impl ScriptedLines {
    fn new(lines: Vec<String>) -> Self {
        Self {
            lines: Rc::new(RefCell::new(lines)),
        }
    }
}

impl Component for ScriptedLines {
    aj_tui::impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<aj_tui::Line> {
        self.lines
            .borrow()
            .iter()
            .cloned()
            .map(aj_tui::Line::from)
            .collect()
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }
}

/// Build N components of K lines each — totalling roughly the
/// 25k-line scrollback we observed on the stuck session.
fn build_scrollback(num_components: usize, lines_per_component: usize) -> Vec<ScriptedLines> {
    (0..num_components)
        .map(|c| {
            let lines = (0..lines_per_component)
                .map(|l| format!("comp {c:04} line {l:04} this is a plausible chat line"))
                .collect();
            ScriptedLines::new(lines)
        })
        .collect()
}

#[test]
#[ignore = "perf bench; opt-in via --ignored"]
fn render_cost_on_long_scrollback() {
    // 250 components × 100 lines ≈ 25k lines, matching the stuck
    // session's `previous_lines.len()`.
    let term = VirtualTerminal::new(200, 40);
    let mut tui = Tui::new(Box::new(term));
    for component in build_scrollback(250, 100) {
        tui.add_child(Box::new(component));
    }

    // Prime the diff engine: first render is a full paint, every
    // subsequent render hits the byte-identical fast path the loader
    // pump would exercise on an otherwise-idle session.
    tui.render();

    const SAMPLES: usize = 50;
    let mut durations_us: Vec<u128> = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let t = Instant::now();
        tui.render();
        durations_us.push(t.elapsed().as_micros());
    }
    durations_us.sort_unstable();
    let median = durations_us[SAMPLES / 2];
    let p10 = durations_us[SAMPLES / 10];
    let p90 = durations_us[SAMPLES * 9 / 10];
    let max = *durations_us.last().unwrap();

    eprintln!(
        "render_cost_on_long_scrollback: lines=~{} samples={SAMPLES} \
         p10={p10}us median={median}us p90={p90}us max={max}us",
        250 * 100,
    );
}
