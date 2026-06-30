//! Vertical spacing component.

use crate::component::{Component, Line};

/// A component that produces empty lines for vertical spacing.
pub struct Spacer {
    lines: usize,
    // Cached blank rows. Returning the same `Line` allocations across
    // frames lets the render engine treat a spacer between two unchanged
    // transcript elements as unchanged and skip re-processing it (see
    // [`Line::same_alloc`]).
    cached: Vec<Line>,
}

impl Spacer {
    /// Create a spacer with `n` blank lines (default: 1).
    pub fn new(lines: usize) -> Self {
        Self {
            lines,
            cached: Vec::new(),
        }
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

    fn render(&mut self, _width: usize) -> Vec<Line> {
        if self.cached.len() != self.lines {
            // All rows share one empty `Rc`; `vec![x; n]` clones the
            // single allocation rather than allocating `n` times.
            self.cached = vec![Line::default(); self.lines];
        }
        self.cached.clone()
    }
}
