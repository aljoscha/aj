//! Regression guards for overlay compositing's handling of SGR resets
//! that sit at or beyond the last visible column.
//!
//! When a base line ends with a trailing reset (e.g. italic-on at column
//! 0, italic-off at the end-of-line reset), the reset itself doesn't
//! occupy a cell but the terminal still needs to know the style is
//! closed before we move onto the next line. A naive slicer that drops
//! the "reset past the last visible column" segment will leave the next
//! line visibly styled.
//!
//! These tests assert that neither the base-only render path nor the
//! overlay-slicing path leaks style attributes onto the next line.

mod support;

use aj_tui::component::Component;
use aj_tui::impl_component_any;
use aj_tui::tui::{OverlayOptions, SizeValue, Tui};

use support::{StaticLines, VirtualTerminal, render_now};

/// One-line overlay, rendered verbatim.
struct SingleLineOverlay(&'static str);

impl Component for SingleLineOverlay {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![self.0.to_string()]
    }
}

fn italic_base_line(width: usize) -> String {
    format!("\x1b[3m{}\x1b[23m", "X".repeat(width))
}

#[test]
fn does_not_leak_italic_into_next_line_without_any_overlay() {
    let width = 20usize;
    let terminal = VirtualTerminal::new(u16::try_from(width).unwrap_or(u16::MAX), 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    tui.add_child(Box::new(StaticLines::new([
        italic_base_line(width),
        "INPUT".to_string(),
    ])));
    render_now(&mut tui);

    let cell = terminal.cell(1, 0).expect("cell (1,0) should exist");
    assert!(
        !cell.italic,
        "italic from row 0 leaked into row 1; cell: {:?}",
        cell,
    );
}

#[test]
fn does_not_leak_italic_into_next_line_with_overlay_slicing() {
    let width = 20usize;
    let terminal = VirtualTerminal::new(u16::try_from(width).unwrap_or(u16::MAX), 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    tui.add_child(Box::new(StaticLines::new([
        italic_base_line(width),
        "INPUT".to_string(),
    ])));

    tui.show_overlay(
        Box::new(SingleLineOverlay("OVR")),
        OverlayOptions {
            row: Some(SizeValue::Absolute(0)),
            col: Some(SizeValue::Absolute(5)),
            width: Some(SizeValue::Absolute(3)),
            ..Default::default()
        },
    );
    render_now(&mut tui);

    let cell = terminal.cell(1, 0).expect("cell (1,0) should exist");
    assert!(
        !cell.italic,
        "italic from row 0 leaked into row 1 after overlay slice; cell: {:?}",
        cell,
    );
}
