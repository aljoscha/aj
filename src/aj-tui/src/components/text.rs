//! Multi-line word-wrapping text display component.

use crate::ansi::{apply_background_to_line, wrap_text_with_ansi};
use crate::component::Component;

/// A component that displays multi-line text with word wrapping.
///
/// Text is word-wrapped to fit the available width minus horizontal padding.
/// Supports optional background color and vertical padding.
pub struct Text {
    text: String,
    padding_x: usize,
    padding_y: usize,
    bg_fn: Option<Box<dyn Fn(&str) -> String>>,
    // Cache.
    cached_text: Option<String>,
    cached_width: Option<usize>,
    cached_lines: Option<Vec<String>>,
}

impl Text {
    /// Create a new Text component with the given content.
    pub fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
            padding_x: 1,
            padding_y: 0,
            bg_fn: None,
            cached_text: None,
            cached_width: None,
            cached_lines: None,
        }
    }

    /// Set the text content.
    pub fn set_text(&mut self, text: &str) {
        if self.text != text {
            self.text = text.to_string();
            self.invalidate_cache();
        }
    }

    /// Get the current text content.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Set horizontal padding (applied on both sides).
    pub fn set_padding_x(&mut self, padding: usize) {
        self.padding_x = padding;
        self.invalidate_cache();
    }

    /// Set vertical padding (applied on top and bottom).
    pub fn set_padding_y(&mut self, padding: usize) {
        self.padding_y = padding;
        self.invalidate_cache();
    }

    /// Set a background color function applied to every line.
    pub fn set_bg_fn(&mut self, bg_fn: Box<dyn Fn(&str) -> String>) {
        self.bg_fn = Some(bg_fn);
        self.invalidate_cache();
    }

    fn invalidate_cache(&mut self) {
        self.cached_text = None;
        self.cached_width = None;
        self.cached_lines = None;
    }
}

impl Component for Text {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        if self.text.is_empty() {
            return Vec::new();
        }

        // Check cache.
        if let (Some(ct), Some(cw), Some(cl)) =
            (&self.cached_text, self.cached_width, &self.cached_lines)
        {
            if ct == &self.text && cw == width {
                return cl.clone();
            }
        }

        let content_width = width.saturating_sub(self.padding_x * 2);
        if content_width == 0 {
            return Vec::new();
        }

        let padding = " ".repeat(self.padding_x);
        let wrapped = wrap_text_with_ansi(&self.text, content_width);

        let mut result = Vec::new();

        // Top padding.
        for _ in 0..self.padding_y {
            let empty = " ".repeat(width);
            result.push(if let Some(ref bg) = self.bg_fn {
                bg(&empty)
            } else {
                empty
            });
        }

        // Content lines.
        for line in &wrapped {
            let padded = format!("{}{}", padding, line);
            let line_out = if let Some(ref bg) = self.bg_fn {
                apply_background_to_line(&padded, width, bg.as_ref())
            } else {
                padded
            };
            result.push(line_out);
        }

        // Bottom padding.
        for _ in 0..self.padding_y {
            let empty = " ".repeat(width);
            result.push(if let Some(ref bg) = self.bg_fn {
                bg(&empty)
            } else {
                empty
            });
        }

        result
    }

    fn invalidate(&mut self) {
        self.invalidate_cache();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_basic() {
        let mut t = Text::new("hello world");
        let lines = t.render(80);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("hello world"));
    }

    #[test]
    fn test_text_wrapping() {
        let mut t = Text::new("hello world");
        t.set_padding_x(0);
        let lines = t.render(6);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_text_empty() {
        let mut t = Text::new("");
        assert!(t.render(80).is_empty());
    }

    #[test]
    fn test_text_padding() {
        let mut t = Text::new("hi");
        t.set_padding_x(2);
        t.set_padding_y(1);
        let lines = t.render(80);
        // 1 top pad + 1 content + 1 bottom pad = 3 lines.
        assert_eq!(lines.len(), 3);
        assert!(lines[1].starts_with("  ")); // 2 chars left padding
    }
}
