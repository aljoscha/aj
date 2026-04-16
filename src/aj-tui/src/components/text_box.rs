//! Container with padding and optional background color.
//!
//! `TextBox` wraps its children in a configurable margin and, when a
//! background-color closure is supplied, paints every row (padding and
//! content alike) through that closure. Caches the composited output
//! across frames so a container whose children return byte-identical
//! lines at the same width doesn't re-allocate padded / bg-applied
//! strings on every render.

use crate::ansi::apply_background_to_line;
use crate::component::Component;
use crate::keys::InputEvent;

/// Rendered state cached between frames when the inputs haven't changed.
///
/// Matched on three axes:
/// - `width`: padding and background application depend on it.
/// - `child_lines`: if any child returned a different line (text
///   changed, style changed, layout changed), the cache is stale.
/// - `bg_sample`: a probe of `bg_fn("test")`. Lets us detect a changed
///   background closure without having to compare function pointers
///   (which Rust doesn't allow for `dyn Fn`). When `bg_fn` is `None`,
///   the sample is `None` too; any flip in the `Option` discriminant
///   misses the cache.
struct RenderCache {
    width: usize,
    child_lines: Vec<String>,
    bg_sample: Option<String>,
    lines: Vec<String>,
}

/// A box container that renders children with padding and an optional background.
pub struct TextBox {
    children: Vec<Box<dyn Component>>,
    padding_x: usize,
    padding_y: usize,
    bg_fn: Option<Box<dyn Fn(&str) -> String>>,
    cache: Option<RenderCache>,
}

impl TextBox {
    pub fn new() -> Self {
        Self {
            children: Vec::new(),
            padding_x: 1,
            padding_y: 1,
            bg_fn: None,
            cache: None,
        }
    }

    /// Add a child component.
    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
        self.cache = None;
    }

    /// Remove a child at the given index.
    pub fn remove_child(&mut self, index: usize) -> Box<dyn Component> {
        self.cache = None;
        self.children.remove(index)
    }

    /// Remove all children.
    pub fn clear(&mut self) {
        self.children.clear();
        self.cache = None;
    }

    /// Set horizontal padding.
    pub fn set_padding_x(&mut self, padding: usize) {
        if padding != self.padding_x {
            self.padding_x = padding;
            self.cache = None;
        }
    }

    /// Set vertical padding.
    pub fn set_padding_y(&mut self, padding: usize) {
        if padding != self.padding_y {
            self.padding_y = padding;
            self.cache = None;
        }
    }

    /// Set a background color function.
    ///
    /// The cache isn't invalidated here: the next render compares a
    /// sampled application of the new closure against the cached
    /// sample and invalidates then. That keeps set-then-render cheap
    /// for identical-output closures (e.g. swapping one `|s| s.into()`
    /// for another) and correct for closures whose output differs.
    pub fn set_bg_fn(&mut self, bg_fn: Box<dyn Fn(&str) -> String>) {
        self.bg_fn = Some(bg_fn);
    }
}

impl Default for TextBox {
    fn default() -> Self {
        Self::new()
    }
}

impl TextBox {
    /// Probe the background closure so we can tell whether the
    /// function's output has changed even when the `Box` pointer
    /// hasn't. An empty probe string (`""`) would let a color-changing
    /// closure that only emits SGR codes land the same sample across
    /// variations; `"test"` is long enough to carry through any
    /// visible-length-sensitive styling too.
    fn bg_sample(&self) -> Option<String> {
        self.bg_fn.as_ref().map(|f| f("test"))
    }
}

impl Component for TextBox {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let content_width = width.saturating_sub(self.padding_x * 2);
        if content_width == 0 || self.children.is_empty() {
            self.cache = None;
            return Vec::new();
        }

        let padding = " ".repeat(self.padding_x);

        // Render children at content width. This is the source of
        // truth for cache freshness: any child that consumed internal
        // state (scroll, focus, streamed input) produces different
        // lines and the equality check below catches that.
        let mut child_lines = Vec::new();
        for child in &mut self.children {
            child_lines.extend(child.render(content_width));
        }

        if child_lines.is_empty() {
            self.cache = None;
            return Vec::new();
        }

        let bg_sample = self.bg_sample();

        // Cache check. Clone out the cached lines only when we hit;
        // callers mutate the returned vector in place (the outer TUI
        // appends `SEGMENT_RESET` to every non-empty line in phase 4
        // of `render`, for example), so we can't hand them a borrowed
        // slice.
        if let Some(cache) = self.cache.as_ref()
            && cache.width == width
            && cache.bg_sample == bg_sample
            && cache.child_lines == child_lines
        {
            return cache.lines.clone();
        }

        let mut lines = Vec::with_capacity(child_lines.len() + 2 * self.padding_y);

        // Top padding.
        for _ in 0..self.padding_y {
            let empty_line = " ".repeat(width);
            lines.push(match &self.bg_fn {
                Some(bg) => bg(&empty_line),
                None => empty_line,
            });
        }

        // Content.
        for child_line in &child_lines {
            let padded = format!("{}{}", padding, child_line);
            lines.push(match &self.bg_fn {
                Some(bg) => apply_background_to_line(&padded, width, bg.as_ref()),
                None => padded,
            });
        }

        // Bottom padding.
        for _ in 0..self.padding_y {
            let empty_line = " ".repeat(width);
            lines.push(match &self.bg_fn {
                Some(bg) => bg(&empty_line),
                None => empty_line,
            });
        }

        self.cache = Some(RenderCache {
            width,
            child_lines,
            bg_sample,
            lines: lines.clone(),
        });

        lines
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        self.cache = None;
        for child in &mut self.children {
            child.invalidate();
        }
    }
}
