//! Container with padding and optional background color.
//!
//! `TextBox` wraps its children in a configurable margin and, when a
//! background-color closure is supplied, paints every row (padding and
//! content alike) through that closure. Caches the composited output
//! across frames so a container whose children return byte-identical
//! lines at the same width doesn't re-allocate padded / bg-applied
//! strings on every render.

use crate::ansi::{apply_background_to_line, visible_width};
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
    /// Construct a TextBox with the given padding. Mirrors pi-tui's
    /// `new Box(paddingX = 1, paddingY = 1, bgFn?)` constructor shape:
    /// padding is supplied at construction and cannot be changed
    /// afterwards. A caller that wants to swap padding mid-life should
    /// build a new `TextBox` and re-attach its children. Pi takes
    /// `bgFn` as an optional third arg with the same shape; the Rust
    /// port keeps `set_bg_fn` for that axis (pi exposes `setBgFn`
    /// too).
    pub fn new(padding_x: usize, padding_y: usize) -> Self {
        Self {
            children: Vec::new(),
            padding_x,
            padding_y,
            bg_fn: None,
            cache: None,
        }
    }

    /// Add a child component.
    pub fn add_child(&mut self, child: Box<dyn Component>) {
        self.children.push(child);
        self.cache = None;
    }

    /// Remove the first child whose identity matches `child` (data-pointer
    /// equality on the underlying component). Mirrors pi-tui's
    /// `Box.removeChild(component: Component)`
    /// (`packages/tui/src/components/box.ts:34`), which uses `indexOf`
    /// on the children array.
    ///
    /// Identity is by [`std::ptr::addr_eq`] on the underlying
    /// `&dyn Component`, so the caller's reference must point to the
    /// same in-memory instance that was added via [`TextBox::add_child`].
    /// Two distinct instances of the same type with byte-identical
    /// fields compare unequal.
    ///
    /// Returns the removed component, or `None` if `child` is not
    /// currently in the box. Pi-tui's `indexOf === -1` branch is a
    /// no-op (cache is *not* invalidated when nothing was removed);
    /// the Rust port follows the same shape — the cache is only
    /// dropped when an actual removal happened.
    ///
    /// The Rust port returns the boxed component (rather than pi's
    /// `void`) so callers can re-attach it elsewhere. Callers that
    /// want pi's discard behavior write
    /// `let _ = tb.remove_child_by_ref(child);`.
    pub fn remove_child_by_ref(&mut self, child: &dyn Component) -> Option<Box<dyn Component>> {
        let target: *const dyn Component = child;
        let index = self.children.iter().position(|c| {
            let ptr: *const dyn Component = c.as_ref();
            std::ptr::addr_eq(ptr, target)
        })?;
        self.cache = None;
        Some(self.children.remove(index))
    }

    /// Remove all children.
    pub fn clear(&mut self) {
        self.children.clear();
        self.cache = None;
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
    /// Default padding `(1, 1)` matches pi-tui's `Box(paddingX = 1,
    /// paddingY = 1, bgFn?)` JS-default-arg shape.
    fn default() -> Self {
        Self::new(1, 1)
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

    /// Pad `line` to `width` columns and, if a background closure is
    /// set, apply it. This is the single helper used by every row of
    /// output (top padding, content rows, bottom padding) so the
    /// padding-and-bg pipeline stays byte-identical across all three
    /// paths. Mirrors pi-tui's private `Box.applyBg` helper.
    ///
    /// The no-bg branch right-pads with spaces too — without it, a
    /// content row whose child returned text shorter than the content
    /// width would render shorter than `width` columns, leaving the
    /// right-side cells at whatever the terminal previously held.
    fn apply_bg_row(&self, line: &str, width: usize) -> String {
        let visible_len = visible_width(line);
        let pad_needed = width.saturating_sub(visible_len);
        let padded = format!("{}{}", line, " ".repeat(pad_needed));
        match &self.bg_fn {
            Some(bg) => apply_background_to_line(&padded, width, bg.as_ref()),
            None => padded,
        }
    }
}

impl Component for TextBox {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        if self.children.is_empty() {
            // Pi-tui parity: empty-children returns `[]` without
            // touching the cache (`add_child` / `remove_child` /
            // `clear` already invalidate, so a subsequent render with
            // children re-installed comes from those mutators rather
            // than this branch). See PORTING.md F34.
            return Vec::new();
        }

        // Clamp content width to at least one cell, mirroring pi-tui's
        // `Math.max(1, width - paddingX * 2)` in `box.ts`. A degenerate
        // render width (`width = 0`, or `width < 2 * padding_x`) still
        // produces one cell of content rather than collapsing the whole
        // box to zero rows. Aligns with the analogous F14 / F40 / F42
        // fixes on `Text` / `Markdown`. See PORTING.md F34.
        let content_width = width.saturating_sub(self.padding_x * 2).max(1);

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
            // Pi-tui parity: leave the cache intact on the empty
            // child-lines branch too. See PORTING.md F34.
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

        // Top padding. Routes through `apply_bg_row` so the padding
        // cells go through the same pad-then-bg pipeline as the
        // content rows.
        for _ in 0..self.padding_y {
            lines.push(self.apply_bg_row("", width));
        }

        // Content. Same `apply_bg_row` path; the helper right-pads
        // shorter rows so the box always renders a full-width
        // rectangle even when no `bg_fn` is set.
        for child_line in &child_lines {
            let padded = format!("{}{}", padding, child_line);
            lines.push(self.apply_bg_row(&padded, width));
        }

        // Bottom padding.
        for _ in 0..self.padding_y {
            lines.push(self.apply_bg_row("", width));
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
