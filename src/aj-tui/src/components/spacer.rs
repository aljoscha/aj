//! Vertical spacing component.

use crate::component::Component;

/// A component that produces empty lines for vertical spacing.
pub struct Spacer {
    lines: usize,
}

impl Spacer {
    /// Create a spacer with `n` blank lines (default: 1).
    pub fn new(lines: usize) -> Self {
        Self { lines }
    }

    /// Set the number of blank lines.
    pub fn set_lines(&mut self, lines: usize) {
        self.lines = lines;
    }
}

impl Default for Spacer {
    fn default() -> Self {
        Self::new(1)
    }
}

impl Component for Spacer {
    crate::impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        vec![String::new(); self.lines]
    }
}
