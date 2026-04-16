//! Overlay rendering when the underlying content is shorter than the
//! terminal. Regression guard: a centered overlay must still paint even if
//! the content below it doesn't extend far enough to "push" the overlay
//! region into the paint pass.

mod support;

use aj_tui::component::Component;
use aj_tui::impl_component_any;
use aj_tui::tui::{OverlayOptions, Tui};

use support::{StaticLines, VirtualTerminal, render_now};

struct SimpleOverlay;

impl Component for SimpleOverlay {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![
            "OVERLAY_TOP".to_string(),
            "OVERLAY_MID".to_string(),
            "OVERLAY_BOT".to_string(),
        ]
    }
}

#[test]
fn renders_overlay_when_content_is_shorter_than_terminal_height() {
    // 24 rows, only 3 lines of content.
    let terminal = VirtualTerminal::new(80, 24);
    let mut tui = Tui::new(Box::new(terminal.clone()));

    tui.add_child(Box::new(StaticLines::new(["Line 1", "Line 2", "Line 3"])));

    // Center anchor by default — around row 10 in a 24-row terminal.
    tui.show_overlay(Box::new(SimpleOverlay), OverlayOptions::default());

    render_now(&mut tui);

    let viewport = terminal.viewport();
    let has_overlay = viewport.iter().any(|line| line.contains("OVERLAY"));

    assert!(
        has_overlay,
        "overlay should be visible when content is shorter than terminal; viewport: {:#?}",
        viewport,
    );
}
