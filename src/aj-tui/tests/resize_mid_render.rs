//! Regression: a terminal resize that lands *between* the start of
//! [`Tui::render`] and the strict line-width validator must not panic.
//!
//! The trigger:
//!
//! 1. `render()` samples `terminal.columns()` at the top and threads
//!    that width through every `Component::render(width)` call.
//! 2. Components (Markdown, Text, others that pad each row to the
//!    render-time width) emit lines that are exactly `width` cells wide.
//! 3. A SIGWINCH lands; the next `terminal.columns()` read returns the
//!    *new*, smaller width.
//! 4. The validator was historically re-reading
//!    `terminal.columns()` and would compare the already-rendered
//!    lines against the new width — tripping
//!    `"rendered line N exceeds terminal width"` on a frame whose lines
//!    were valid for the width they were rendered at.
//!
//! The fix snapshots the dimensions once at the top of `render()` and
//! threads that snapshot through every later reader (validator,
//! strategy decision, deletion-cleanup math). This test simulates the
//! race by wrapping a [`VirtualTerminal`] with a column reader that
//! flips its reported value the moment the engine asks for it the
//! second time within a single render pass.

use aj_tui_testkit as support;

use std::cell::Cell;
use std::io;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::terminal::Terminal;
use aj_tui::tui::Tui;

use support::VirtualTerminal;

/// A component whose `render(width)` always emits one line of exactly
/// `width` cells of `x`. That gives the validator a hard contract:
/// "the line is exactly the width you asked for", so any later read of
/// `terminal.columns()` that returns a smaller number would have
/// historically tripped the strict line-width check.
struct FillToWidth;

impl Component for FillToWidth {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        vec!["x".repeat(width)]
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }
}

/// `Terminal` wrapper that delegates everything to an inner
/// `VirtualTerminal` but lies about `columns()` on the second call
/// within a single render pass.
///
/// The shared counter is reset to zero by the test before each
/// `tui.render()` call so the simulated SIGWINCH only fires once per
/// render — modelling the real-world case where the OS reports a new
/// size after a single `ioctl(TIOCGWINSZ)`.
struct ShrinkOnSecondColumnsRead {
    inner: VirtualTerminal,
    counter: Rc<Cell<u32>>,
    shrunk_width: u16,
}

impl ShrinkOnSecondColumnsRead {
    fn new(inner: VirtualTerminal, counter: Rc<Cell<u32>>, shrunk_width: u16) -> Self {
        Self {
            inner,
            counter,
            shrunk_width,
        }
    }
}

impl Terminal for ShrinkOnSecondColumnsRead {
    fn write(&mut self, data: &str) {
        self.inner.write(data);
    }

    fn start(&mut self) -> io::Result<()> {
        self.inner.start()
    }

    fn stop(&mut self) {
        self.inner.stop();
    }

    fn columns(&self) -> u16 {
        let n = self.counter.get();
        self.counter.set(n + 1);
        if n == 0 {
            // First read in the render pass: report the real (wide)
            // size. The Tui hands this to every component as its render
            // width.
            self.inner.columns()
        } else {
            // Every subsequent read in the same render pass: pretend a
            // SIGWINCH already shrank us. The fix must NOT consult this
            // value when validating, otherwise the test panics.
            self.shrunk_width
        }
    }

    fn rows(&self) -> u16 {
        self.inner.rows()
    }

    fn move_by(&mut self, lines: i32) {
        self.inner.move_by(lines);
    }

    fn hide_cursor(&mut self) {
        self.inner.hide_cursor();
    }

    fn show_cursor(&mut self) {
        self.inner.show_cursor();
    }

    fn clear_line(&mut self) {
        self.inner.clear_line();
    }

    fn clear_from_cursor(&mut self) {
        self.inner.clear_from_cursor();
    }

    fn clear_screen(&mut self) {
        self.inner.clear_screen();
    }

    fn set_title(&mut self, title: &str) {
        self.inner.set_title(title);
    }

    fn flush(&mut self) {
        self.inner.flush();
    }
}

#[test]
fn mid_render_resize_does_not_trip_line_width_validator() {
    let virt = VirtualTerminal::new(200, 24);
    let counter = Rc::new(Cell::new(0u32));
    let term = ShrinkOnSecondColumnsRead::new(virt.clone(), Rc::clone(&counter), 80);

    let mut tui = Tui::new(Box::new(term));
    assert!(
        tui.strict_line_widths(),
        "strict line-width validation is on by default; the regression \
         it guards is exactly what this test exercises",
    );
    tui.add_child(Box::new(FillToWidth));

    // Render once with the counter pre-armed: the engine reads
    // `terminal.columns()` at the top of `render()` (returning 200),
    // then every later read in the same pass returns 80. The component
    // renders a 200-wide line. Pre-fix, the validator would re-read
    // `terminal.columns()` (getting 80) and panic with
    // `"rendered line N exceeds terminal width (200 > 80)"`. Post-fix,
    // the validator uses the snapshotted 200 and the frame goes
    // through.
    counter.set(0);
    tui.render();

    // No panic ⇒ the snapshot path is in effect. Sanity-check the
    // render counters so a future refactor that silently no-ops
    // `render()` doesn't slip through.
    assert_eq!(
        tui.total_renders(),
        1,
        "the render call should still have completed end-to-end",
    );
}
