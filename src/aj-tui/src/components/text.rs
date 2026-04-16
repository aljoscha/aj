//! Multi-line word-wrapping text display component.

use crate::ansi::{apply_background_to_line, visible_width, wrap_text_with_ansi};
use crate::component::Component;

/// Default horizontal padding (left/right margin) applied to text content.
/// Matches pi-tui's `Text` constructor default.
const DEFAULT_PADDING_X: usize = 1;

/// Default vertical padding (top/bottom blank rows) around text content.
/// Matches pi-tui's `Text` constructor default.
const DEFAULT_PADDING_Y: usize = 1;

/// Tabs in input text are normalized to this many spaces before wrapping,
/// matching pi-tui's `text.replace(/\t/g, "   ")` step.
const TAB_AS_SPACES: &str = "   ";

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
    /// Construct a Text component with the given content and padding.
    /// Mirrors pi-tui's `new Text(text = "", paddingX = 1, paddingY = 1,
    /// customBgFn?)` constructor shape: padding is supplied at
    /// construction and cannot be changed afterwards. A caller that wants
    /// to swap padding mid-life should build a new `Text`. Pi exposes
    /// `customBgFn` as an optional fourth arg with the same shape; the
    /// Rust port keeps `set_bg_fn` for that axis (pi exposes
    /// `setCustomBgFn` too). See PORTING.md F49.
    pub fn new(text: &str, padding_x: usize, padding_y: usize) -> Self {
        Self {
            text: text.to_string(),
            padding_x,
            padding_y,
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

impl Default for Text {
    /// Default `("", 1, 1)` matches pi-tui's `Text(text = "",
    /// paddingX = 1, paddingY = 1, customBgFn?)` JS-default-arg shape.
    fn default() -> Self {
        Self::new("", DEFAULT_PADDING_X, DEFAULT_PADDING_Y)
    }
}

impl Component for Text {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Check cache.
        if let (Some(ct), Some(cw), Some(cl)) =
            (&self.cached_text, self.cached_width, &self.cached_lines)
        {
            if ct == &self.text && cw == width {
                return cl.clone();
            }
        }

        // No content to render. pi-tui treats both empty and
        // whitespace-only text as "nothing to draw" and returns an empty
        // vec — no top/bottom padding rows are emitted.
        if self.text.is_empty() || self.text.trim().is_empty() {
            self.cached_text = Some(self.text.clone());
            self.cached_width = Some(width);
            self.cached_lines = Some(Vec::new());
            return Vec::new();
        }

        // Replace tabs with three spaces before wrapping, matching
        // pi-tui. This keeps tab-indented input from rendering as a
        // single literal `\t` cell with the wrong visible width.
        let normalized: String = self.text.replace('\t', TAB_AS_SPACES);

        // pi-tui clamps content_width to at least 1 so a degenerate
        // `width <= 2 * padding_x` doesn't drop the row entirely.
        let content_width = std::cmp::max(1, width.saturating_sub(self.padding_x * 2));

        let left_margin = " ".repeat(self.padding_x);
        let right_margin = " ".repeat(self.padding_x);
        let wrapped = wrap_text_with_ansi(&normalized, content_width);

        // Content lines: `leftMargin + line + rightMargin`, then pad with
        // spaces to reach the full terminal width. pi-tui applies the
        // right margin and the trailing pad even in the no-bg case so
        // every content row spans the full render width — the previous
        // port emitted `leftMargin + line` only and left the right side
        // of each row at whatever width the wrapped text happened to
        // produce.
        let mut content_lines: Vec<String> = Vec::with_capacity(wrapped.len());
        for line in &wrapped {
            let line_with_margins = format!("{}{}{}", left_margin, line, right_margin);
            let line_out = if let Some(ref bg) = self.bg_fn {
                apply_background_to_line(&line_with_margins, width, bg.as_ref())
            } else {
                let visible_len = visible_width(&line_with_margins);
                let padding_needed = width.saturating_sub(visible_len);
                format!("{}{}", line_with_margins, " ".repeat(padding_needed))
            };
            content_lines.push(line_out);
        }

        // Top / bottom padding rows. pi-tui hoists `emptyLine` outside
        // the loop and reuses it for each row; the bg branch routes
        // through `applyBackgroundToLine` (no-op padding pass since the
        // line already spans the full width, then bg_fn).
        let empty_line = " ".repeat(width);
        let mut empty_lines: Vec<String> = Vec::with_capacity(self.padding_y);
        for _ in 0..self.padding_y {
            let line = if let Some(ref bg) = self.bg_fn {
                apply_background_to_line(&empty_line, width, bg.as_ref())
            } else {
                empty_line.clone()
            };
            empty_lines.push(line);
        }

        let mut result: Vec<String> =
            Vec::with_capacity(content_lines.len() + 2 * empty_lines.len());
        result.extend(empty_lines.iter().cloned()); // Top padding.
        result.extend(content_lines);
        result.extend(empty_lines); // Bottom padding (consumes empty_lines).

        // Cache the pre-fallback `result`. pi-tui's tail expression
        // `result.length > 0 ? result : [""]` only fires on the *current*
        // render; the cache stores the original (possibly empty)
        // `result`. A subsequent render with the same args therefore
        // returns the cached vec verbatim. The fallback is defensive
        // against a hypothetical wrap output of zero lines, but in
        // practice both pi's and our `wrap_text_with_ansi` always return
        // at least one line for any input that passed the early
        // empty/whitespace check, so this branch is unreachable for real
        // inputs. We keep it for byte-level parity with pi.
        self.cached_text = Some(self.text.clone());
        self.cached_width = Some(width);
        self.cached_lines = Some(result.clone());

        // Tail fallback: a non-empty input that nonetheless produced no
        // rows returns a single blank row this call. Subsequent
        // cache-hits return the (empty) cached vec, matching pi's
        // first-vs-cached asymmetry.
        if result.is_empty() {
            return vec![String::new()];
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
        let mut t = Text::new("hello world", 1, 1);
        // `padding_y = 1` produces top/bottom blank rows around the
        // single content row.
        let lines = t.render(80);
        assert_eq!(lines.len(), 3);
        assert!(lines[1].contains("hello world"));
    }

    #[test]
    fn test_text_wrapping() {
        let mut t = Text::new("hello world", 0, 0);
        let lines = t.render(6);
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn test_text_empty() {
        let mut t = Text::new("", 1, 1);
        assert!(t.render(80).is_empty());
    }

    #[test]
    fn test_text_padding() {
        let mut t = Text::new("hi", 2, 1);
        let lines = t.render(80);
        // 1 top pad + 1 content + 1 bottom pad = 3 lines.
        assert_eq!(lines.len(), 3);
        assert!(lines[1].starts_with("  ")); // 2 chars left padding
    }
}
