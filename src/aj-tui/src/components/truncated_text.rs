//! Single-line truncating text component.

use crate::ansi::truncate_to_width;
use crate::component::{Component, Line};

/// Default horizontal padding (left/right margin) applied to text content.
const DEFAULT_PADDING_X: usize = 0;

/// Default vertical padding (top/bottom blank rows) around text content.
const DEFAULT_PADDING_Y: usize = 0;

/// A component that displays a single line of text, truncated with an
/// ellipsis if it exceeds the available width.
///
/// Useful for status bars, breadcrumbs, and other single-line displays.
///
/// All fields are supplied at construction. There are no setters: a
/// caller that wants to swap text or padding mid-life must build a new
/// `TruncatedText`.
pub struct TruncatedText {
    text: String,
    padding_x: usize,
    padding_y: usize,
}

impl TruncatedText {
    /// Construct a `TruncatedText` with the given content and padding.
    /// Padding is supplied at construction and cannot be changed; build
    /// a new instance to vary it.
    pub fn new(text: &str, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text: text.to_string(),
            padding_x,
            padding_y,
        }
    }

    /// Get the current text.
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Default for TruncatedText {
    fn default() -> Self {
        Self::new("", DEFAULT_PADDING_X, DEFAULT_PADDING_Y)
    }
}

impl Component for TruncatedText {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<Line> {
        let empty_line = " ".repeat(width);

        let mut result = Vec::new();

        // Top vertical padding: full-width blank lines.
        for _ in 0..self.padding_y {
            result.push(empty_line.clone());
        }

        // Available content width after horizontal padding on both sides.
        let available_width = width.saturating_sub(self.padding_x * 2).max(1);

        // Take only the first line (stop at the first newline).
        let first_line = self.text.lines().next().unwrap_or("");

        // Truncate without padding here; we pad the whole line once at the
        // end so right-side padding reaches the full target width.
        let display_text = truncate_to_width(first_line, available_width, "...", false);

        let horiz_padding = " ".repeat(self.padding_x);
        let line_with_padding = format!("{}{}{}", horiz_padding, display_text, horiz_padding);

        let visible = crate::ansi::visible_width(&line_with_padding);
        let extra = width.saturating_sub(visible);
        let final_line = format!("{}{}", line_with_padding, " ".repeat(extra));
        result.push(final_line);

        // Bottom vertical padding: full-width blank lines.
        for _ in 0..self.padding_y {
            result.push(empty_line.clone());
        }

        result.into_iter().map(Line::from).collect()
    }
}
