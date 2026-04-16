//! Single-line truncating text component.

use crate::ansi::truncate_to_width;
use crate::component::Component;

/// Default horizontal padding (left/right margin) applied to text content.
/// Matches pi-tui's `TruncatedText` constructor default.
const DEFAULT_PADDING_X: usize = 0;

/// Default vertical padding (top/bottom blank rows) around text content.
/// Matches pi-tui's `TruncatedText` constructor default.
const DEFAULT_PADDING_Y: usize = 0;

/// A component that displays a single line of text, truncated with an
/// ellipsis if it exceeds the available width.
///
/// Useful for status bars, breadcrumbs, and other single-line displays.
///
/// # Constructor shape
///
/// Mirrors pi-tui's
/// `new TruncatedText(text, paddingX = 0, paddingY = 0)` shape: every
/// field is supplied at construction and there are **no setters**. Pi's
/// `TruncatedText` is fully immutable — a caller that wants to swap
/// text or padding mid-life must build a new `TruncatedText`. The Rust
/// port mirrors that contract byte-for-byte. See PORTING.md F49.
pub struct TruncatedText {
    text: String,
    padding_x: usize,
    padding_y: usize,
}

impl TruncatedText {
    /// Construct a `TruncatedText` with the given content and padding.
    /// Mirrors pi-tui's
    /// `new TruncatedText(text, paddingX = 0, paddingY = 0)` constructor.
    /// Padding is supplied at construction and cannot be changed; build
    /// a new instance to vary it. See PORTING.md F49.
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
    /// Default `("", 0, 0)` matches pi-tui's
    /// `TruncatedText(text, paddingX = 0, paddingY = 0)` JS-default-arg
    /// shape.
    fn default() -> Self {
        Self::new("", DEFAULT_PADDING_X, DEFAULT_PADDING_Y)
    }
}

impl Component for TruncatedText {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
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

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::ansi::visible_width;

    #[test]
    fn test_truncated_text_fits() {
        let mut t = TruncatedText::new("hello", 0, 0);
        let lines = t.render(80);
        assert_eq!(lines.len(), 1);
        // Padded to 80 chars.
        assert_eq!(visible_width(&lines[0]), 80);
    }

    #[test]
    fn test_truncated_text_truncates() {
        let mut t = TruncatedText::new("hello world this is long", 0, 0);
        let lines = t.render(10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("..."));
    }

    #[test]
    fn test_truncated_text_multiline_takes_first() {
        let mut t = TruncatedText::new("first\nsecond", 0, 0);
        let lines = t.render(80);
        assert!(lines[0].contains("first"));
        assert!(!lines[0].contains("second"));
    }

    #[test]
    fn default_constructs_with_zero_padding() {
        // Mirrors pi-tui's `TruncatedText(text, paddingX = 0, paddingY = 0)`
        // JS-default-arg shape.
        let mut t = TruncatedText::default();
        let lines = t.render(40);
        assert_eq!(lines.len(), 1);
        assert_eq!(visible_width(&lines[0]), 40);
    }
}
