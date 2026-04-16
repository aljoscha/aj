//! Single-line truncating text component.

use crate::ansi::truncate_to_width;
use crate::component::Component;

/// A component that displays a single line of text, truncated with an
/// ellipsis if it exceeds the available width.
///
/// Useful for status bars, breadcrumbs, and other single-line displays.
pub struct TruncatedText {
    text: String,
    padding_x: usize,
    padding_y: usize,
}

impl TruncatedText {
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            padding_x: 0,
            padding_y: 0,
        }
    }

    /// Set the text content.
    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
    }

    /// Get the current text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Set horizontal padding.
    pub fn set_padding_x(&mut self, padding: usize) {
        self.padding_x = padding;
    }

    /// Set vertical padding.
    pub fn set_padding_y(&mut self, padding: usize) {
        self.padding_y = padding;
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
        let mut t = TruncatedText::new("hello");
        let lines = t.render(80);
        assert_eq!(lines.len(), 1);
        // Padded to 80 chars.
        assert_eq!(visible_width(&lines[0]), 80);
    }

    #[test]
    fn test_truncated_text_truncates() {
        let mut t = TruncatedText::new("hello world this is long");
        let lines = t.render(10);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("..."));
    }

    #[test]
    fn test_truncated_text_multiline_takes_first() {
        let mut t = TruncatedText::new("first\nsecond");
        let lines = t.render(80);
        assert!(lines[0].contains("first"));
        assert!(!lines[0].contains("second"));
    }
}
