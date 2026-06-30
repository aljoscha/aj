//! `Tui::validate_line_widths` short-circuits on rows that are
//! byte-identical to the previous frame at the same layout width.
//! These tests pin the two corners of that optimization:
//!
//! 1. The skip is *safe* when the input genuinely hasn't changed:
//!    two renders of identical content at the same width succeed.
//! 2. The skip does *not* mask a fresh overflow introduced on the
//!    second render — a row whose bytes differ from the previous
//!    frame must still be measured.
//! 3. The skip does *not* mask a width *shrink*: even if the rendered
//!    bytes are identical, a narrower terminal must re-validate every
//!    row.
//!
//! The optimization exists because, on a long conversation, the
//! per-render width check was dominating CPU (the spinner pumped a
//! render every 80ms and the validator re-walked every byte of the
//! whole frame each time). Cutting it back to "only validate rows that
//! the diff engine would also repaint" keeps the safety contract
//! intact for the rows that matter.
//!
//! See [`Tui::validate_line_widths`] in `src/aj-tui/src/tui.rs`.

use aj_tui_testkit as support;

use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::terminal::Terminal;
use aj_tui::tui::Tui;

use support::VirtualTerminal;

/// A component that emits a fixed list of lines on each render. The
/// `Vec<String>` lives in a `Rc<RefCell<…>>` so a test can swap it
/// between renders to model "content changed" or "content unchanged".
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

    fn set(&self, lines: Vec<String>) {
        *self.lines.borrow_mut() = lines;
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

/// Two renders of byte-identical valid content succeed. This is the
/// happy path the optimization targets — the second render must not
/// panic and must not silently regress.
#[test]
fn repeated_renders_of_identical_content_succeed() {
    let virt = VirtualTerminal::new(40, 10);
    let mut tui = Tui::new(Box::new(virt));
    assert!(
        tui.strict_line_widths(),
        "strict validation is on by default",
    );
    tui.add_child(Box::new(ScriptedLines::new(vec![
        "hello".to_string(),
        "world".to_string(),
    ])));

    tui.render();
    tui.render();

    assert_eq!(tui.total_renders(), 2);
}

/// A second render that introduces a fresh overflow on a row whose
/// bytes differ from the previous frame must still panic. The
/// optimization keys on byte-equality, so a changed row falls through
/// to the full `visible_width` check.
#[test]
#[should_panic(expected = "exceeds terminal width")]
fn second_render_with_a_new_overflowing_row_still_panics() {
    let virt = VirtualTerminal::new(20, 10);
    let scripted = ScriptedLines::new(vec!["short".to_string()]);
    let handle = scripted.clone();

    let mut tui = Tui::new(Box::new(virt));
    tui.add_child(Box::new(scripted));

    // First render establishes `previous_lines = ["short"]` at width
    // 20 — well under the limit.
    tui.render();

    // Swap in a row that is wider than the terminal. Its bytes differ
    // from the previous frame's `"short"`, so the skip condition does
    // not apply and the validator must measure it and panic.
    handle.set(vec!["x".repeat(40)]);
    tui.render();
}

/// A terminal that reports a shrinking width on successive renders —
/// not mid-render — so the optimization sees `previous_width != width`
/// on the second render and is forced to re-walk every row, including
/// rows whose bytes are unchanged from the previous frame.
struct ShrinkingTerminal {
    inner: VirtualTerminal,
    width_override: Rc<Cell<u16>>,
}

impl Terminal for ShrinkingTerminal {
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
        self.width_override.get()
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

/// A width shrink between two renders must re-validate every row,
/// even those whose bytes are byte-identical to the previous frame:
/// rows that were fine at the old width can overflow the new one.
#[test]
#[should_panic(expected = "exceeds terminal width")]
fn width_shrink_re_validates_byte_identical_rows() {
    let virt = VirtualTerminal::new(40, 10);
    let width_override = Rc::new(Cell::new(40u16));
    let term = ShrinkingTerminal {
        inner: virt,
        width_override: Rc::clone(&width_override),
    };

    // A row that's exactly 30 cells wide: fits comfortably at width 40,
    // overflows width 20.
    let mut tui = Tui::new(Box::new(term));
    tui.add_child(Box::new(ScriptedLines::new(vec!["x".repeat(30)])));

    // First render at width 40 — passes validation, populating
    // `previous_lines` and `previous_width = 40`.
    tui.render();

    // Shrink the terminal. The component still emits the same 30-cell
    // row, but the snapshotted layout width on the next render is now
    // 20. The skip condition checks `previous_width == width` (40 ==
    // 20 is false), so every row is re-measured and the validator
    // panics.
    width_override.set(20);
    tui.render();
}
