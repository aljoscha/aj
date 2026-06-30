//! End-to-end guard for the render pipeline's control-character hygiene.
//!
//! A component's rendered row is contractually one visual line. If a
//! component slips a raw newline or tab into a row, the engine must flatten
//! it before it reaches the terminal: the newline is dropped (the row stays
//! a single line instead of spilling onto the next and corrupting the diff
//! engine's row tracking) and the tab expands to spaces.

use aj_tui_testkit as support;

use aj_tui::component::Component;
use aj_tui::impl_component_any;
use aj_tui::tui::Tui;

use support::{VirtualTerminal, render_now};

/// Component that returns whatever rows it was handed, control characters
/// and all.
struct DirtyLines(Vec<String>);

impl Component for DirtyLines {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<aj_tui::Line> {
        self.0.iter().cloned().map(aj_tui::Line::from).collect()
    }
}

#[test]
fn embedded_newline_does_not_split_into_extra_rows() {
    let terminal = VirtualTerminal::new(40, 6);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.add_child(Box::new(DirtyLines(vec![
        "alpha\nbeta".to_string(),
        "gamma".to_string(),
    ])));
    render_now(&mut tui);

    // The newline is dropped, so the two source rows stay two rows: the
    // first is flattened rather than spilling "beta" onto its own line and
    // pushing "gamma" down.
    assert_eq!(
        terminal.viewport_trimmed(),
        vec!["alphabeta".to_string(), "gamma".to_string()],
    );
}

#[test]
fn raw_tab_expands_to_spaces() {
    let terminal = VirtualTerminal::new(40, 4);
    let mut tui = Tui::new(Box::new(terminal.clone()));
    tui.add_child(Box::new(DirtyLines(vec!["a\tb".to_string()])));
    render_now(&mut tui);

    // The tab is rendered as the spaces the width math assumed, so the
    // terminal's own tab stops can't drift it out of alignment.
    assert_eq!(terminal.viewport_trimmed(), vec!["a   b".to_string()]);
}
