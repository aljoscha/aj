//! Titled, bordered overlay container.
//!
//! Wraps a child component with rounded-box chrome and delegates all
//! input, focus, and lifecycle behavior to the child. The wrapper is
//! purely visual.

use std::sync::Arc;

use crate::ansi::{truncate_to_width, visible_width};
use crate::component::Component;
use crate::impl_component_any;
use crate::keys::InputEvent;
use crate::tui::RenderHandle;

/// A titled, bordered overlay container. Wraps any child component
/// with a rounded box border and delegates input/focus to the child.
///
/// `OverlayWindow` adds no input handling of its own — Esc, navigation,
/// and confirm all belong to the child component.
///
/// The frame renders to a stable height: the child region is always
/// padded (or truncated) to exactly `inner_rows` lines so the overlay
/// stops jittering as the child's natural row count changes (e.g. the
/// command palette narrowing its filtered list as the user types).
/// Total rendered height is always `inner_rows + 4`:
/// `top_border + top_padding + inner_rows + bottom_padding + bottom_border`.
///
/// `inner_rows` is either fixed at construction or recomputed each frame
/// from the live terminal height (see [`InnerHeight`]); either way the
/// child is told the resolved height via
/// [`Component::set_available_height`] before it renders, then its
/// output is padded/truncated to match.
pub struct OverlayWindow {
    title: String,
    /// Optional bottom-border subtitle (e.g. a key-hint). Right-aligned
    /// on the bottom edge, mirroring the title-on-top-border layout.
    subtitle: Option<Subtitle>,
    child: Box<dyn Component>,
    theme: OverlayWindowTheme,
    height: InnerHeight,
}

/// Source of the bottom-border subtitle text.
enum Subtitle {
    /// Constant text set at construction.
    Static(String),
    /// Recomputed each frame from the child component, so the hint can
    /// track child state (e.g. a submenu changing what the keys do).
    Dynamic(Box<dyn Fn(&dyn Component) -> String>),
}

/// How the [`OverlayWindow`]'s inner content height is determined.
enum InnerHeight {
    /// Constant row budget set at construction.
    Fixed(usize),
    /// Recomputed each frame as `policy(handle.terminal_rows())`, so the
    /// box tracks terminal resizes. The window stays policy-agnostic:
    /// the height rule (percentages, clamps, chrome accounting) lives
    /// entirely in the injected closure.
    Dynamic {
        handle: RenderHandle,
        policy: Box<dyn Fn(usize) -> usize>,
    },
}

/// Theme closures for the overlay frame. Assembled by the agent layer
/// from its palette; the tui crate stays palette-agnostic. Closures
/// use `Arc` so a single theme can be cloned cheaply into multiple
/// overlays.
#[derive(Clone)]
pub struct OverlayWindowTheme {
    /// Style for the border characters (corners, edges).
    pub border: Arc<dyn Fn(&str) -> String>,
    /// Style for the inline title text.
    pub title: Arc<dyn Fn(&str) -> String>,
    /// Style for the inline bottom-border subtitle (typically dim
    /// hint text).
    pub subtitle: Arc<dyn Fn(&str) -> String>,
}

impl OverlayWindow {
    /// Build a new overlay frame around `child` with the given `title`.
    ///
    /// `inner_rows` is the fixed row count the child region is padded /
    /// truncated to. Total rendered height is `inner_rows + 4` (top
    /// border, top blank padding, child rows, bottom blank padding,
    /// bottom border).
    pub fn new(
        title: impl Into<String>,
        child: Box<dyn Component>,
        theme: OverlayWindowTheme,
        inner_rows: usize,
    ) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            child,
            theme,
            height: InnerHeight::Fixed(inner_rows),
        }
    }

    /// Switch the inner-row budget to dynamic mode: each frame the
    /// budget is `policy(handle.terminal_rows())`, so the box grows and
    /// shrinks with the terminal. The window applies the policy verbatim
    /// — clamps and chrome accounting belong in the caller's closure.
    pub fn with_dynamic_height(
        mut self,
        handle: RenderHandle,
        policy: impl Fn(usize) -> usize + 'static,
    ) -> Self {
        self.height = InnerHeight::Dynamic {
            handle,
            policy: Box::new(policy),
        };
        self
    }

    /// Resolve this frame's inner-row budget from the current height mode.
    fn inner_rows(&self) -> usize {
        match &self.height {
            InnerHeight::Fixed(n) => *n,
            InnerHeight::Dynamic { handle, policy } => policy(usize::from(handle.terminal_rows())),
        }
    }

    /// Set a bottom-border subtitle. Usually a short navigation hint
    /// like `"Enter to confirm  •  Esc to close"`. Same truncation
    /// policy as the title; omitted entirely when the box is too
    /// narrow to fit it.
    pub fn with_subtitle(mut self, subtitle: impl Into<String>) -> Self {
        self.subtitle = Some(Subtitle::Static(subtitle.into()));
        self
    }

    /// Set a bottom-border subtitle that is re-resolved from the child
    /// every frame. Use this when the right hint depends on the child's
    /// state — the closure typically downcasts the child via `as_any`
    /// and picks a hint accordingly. Same truncation policy as
    /// [`OverlayWindow::with_subtitle`].
    pub fn with_dynamic_subtitle(
        mut self,
        subtitle: impl Fn(&dyn Component) -> String + 'static,
    ) -> Self {
        self.subtitle = Some(Subtitle::Dynamic(Box::new(subtitle)));
        self
    }

    /// Build the top edge: `╭─ title ─…─╮`, with the title inset two
    /// characters from the left and a single space gap on each side.
    /// When the width is too small to fit the title and its gaps, the
    /// title is truncated; in the degenerate case where even one
    /// title character won't fit, the title is omitted entirely.
    fn render_top(&self, width: usize) -> String {
        self.render_edge(
            width,
            '╭',
            '╮',
            self.title.as_str(),
            &self.theme.title,
            false,
        )
    }

    fn render_bottom(&self, width: usize) -> String {
        let subtitle = match &self.subtitle {
            None => String::new(),
            Some(Subtitle::Static(text)) => text.clone(),
            Some(Subtitle::Dynamic(resolve)) => resolve(self.child.as_ref()),
        };
        self.render_edge(width, '╰', '╯', &subtitle, &self.theme.subtitle, true)
    }

    /// Shared top/bottom edge renderer. `right_align` controls inline
    /// label placement: `false` insets the label two columns from the
    /// left corner (title), `true` insets it two columns from the
    /// right corner (subtitle).
    fn render_edge(
        &self,
        width: usize,
        left_corner: char,
        right_corner: char,
        label: &str,
        label_style: &Arc<dyn Fn(&str) -> String>,
        right_align: bool,
    ) -> String {
        let border = &self.theme.border;
        if width < 2 {
            return border(&"─".repeat(width));
        }
        let interior = width - 2;
        let label_vw = visible_width(label);
        // Need at least `─ x ─` = 5 interior cols for a one-char label.
        if label.is_empty() || interior < 5 {
            return format!(
                "{}{}{}",
                border(&left_corner.to_string()),
                border(&"─".repeat(interior)),
                border(&right_corner.to_string()),
            );
        }
        let max_label = interior.saturating_sub(4);
        let shown = if label_vw <= max_label {
            label.to_string()
        } else {
            truncate_to_width(label, max_label, "…", false)
        };
        let shown_vw = visible_width(&shown);
        let trailing = interior - 4 - shown_vw;
        let styled = label_style(&format!(" {shown} "));
        if right_align {
            format!(
                "{}{}{}{}{}{}",
                border(&left_corner.to_string()),
                border(&"─".repeat(trailing)),
                border("─"),
                styled,
                border("─"),
                border(&right_corner.to_string()),
            )
        } else {
            format!(
                "{}{}{}{}{}{}",
                border(&left_corner.to_string()),
                border("─"),
                styled,
                border("─"),
                border(&"─".repeat(trailing)),
                border(&right_corner.to_string()),
            )
        }
    }

    /// Wrap a single child line in `│ … │`, padding/truncating to fit
    /// `inner_width` visible columns.
    fn wrap_inner(&self, line: &str, inner_width: usize) -> String {
        let border = &self.theme.border;
        let line_vw = visible_width(line);
        let padded = if line_vw == inner_width {
            line.to_string()
        } else if line_vw < inner_width {
            format!("{}{}", line, " ".repeat(inner_width - line_vw))
        } else {
            truncate_to_width(line, inner_width, "", true)
        };
        format!("{} {} {}", border("│"), padded, border("│"))
    }
}

impl Component for OverlayWindow {
    impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Degenerate widths: hand back something minimal but valid.
        if width < 4 {
            return vec!["".to_string()];
        }
        let inner_width = width - 4;
        let inner_rows = self.inner_rows();
        // Push the resolved budget down before rendering so the child can
        // size its scroll regions to match.
        self.child.set_available_height(inner_rows);
        let mut child_lines = self.child.render(inner_width);

        // Stabilize child output to exactly `inner_rows` rows: pad
        // short children with blanks; clip oversized children (callers
        // should tune `max_visible` so this branch is dead).
        if child_lines.len() > inner_rows {
            child_lines.truncate(inner_rows);
        }

        let blank = " ".repeat(inner_width);
        let mut out = Vec::with_capacity(inner_rows + 4);
        out.push(self.render_top(width));
        out.push(self.wrap_inner(&blank, inner_width));
        let rendered = child_lines.len();
        for line in &child_lines {
            out.push(self.wrap_inner(line, inner_width));
        }
        for _ in rendered..inner_rows {
            out.push(self.wrap_inner(&blank, inner_width));
        }
        out.push(self.wrap_inner(&blank, inner_width));
        out.push(self.render_bottom(width));
        out
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        self.child.handle_input(event)
    }

    fn wants_key_release(&self) -> bool {
        self.child.wants_key_release()
    }

    fn invalidate(&mut self) {
        self.child.invalidate();
    }

    fn set_focused(&mut self, focused: bool) {
        self.child.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.child.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::Any;

    /// Identity theme: pass strings through verbatim so structural
    /// assertions can match on literal characters.
    fn identity_theme() -> OverlayWindowTheme {
        OverlayWindowTheme {
            border: Arc::new(|s: &str| s.to_string()),
            title: Arc::new(|s: &str| s.to_string()),
            subtitle: Arc::new(|s: &str| s.to_string()),
        }
    }

    struct MockChild {
        lines: Vec<String>,
        input_count: usize,
        focused: bool,
        last_width: usize,
        last_available_height: Option<usize>,
    }

    impl MockChild {
        fn new(lines: Vec<&str>) -> Self {
            Self {
                lines: lines.into_iter().map(String::from).collect(),
                input_count: 0,
                focused: false,
                last_width: 0,
                last_available_height: None,
            }
        }
    }

    impl Component for MockChild {
        fn render(&mut self, width: usize) -> Vec<String> {
            self.last_width = width;
            self.lines.clone()
        }
        fn handle_input(&mut self, _event: &InputEvent) -> bool {
            self.input_count += 1;
            true
        }
        fn set_available_height(&mut self, rows: usize) {
            self.last_available_height = Some(rows);
        }
        fn set_focused(&mut self, focused: bool) {
            self.focused = focused;
        }
        fn is_focused(&self) -> bool {
            self.focused
        }
        fn as_any(&self) -> &dyn Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    #[test]
    fn title_renders_inline_on_top_border() {
        let child = Box::new(MockChild::new(vec!["hello"]));
        let mut ov = OverlayWindow::new("Test", child, identity_theme(), 5);
        let lines = ov.render(20);
        let top = &lines[0];
        assert!(top.starts_with('╭'), "top starts with ╭: {top:?}");
        assert!(top.ends_with('╮'), "top ends with ╮: {top:?}");
        assert!(top.contains("Test"), "top contains title: {top:?}");
        assert_eq!(visible_width(top), 20);
    }

    #[test]
    fn child_content_padded_into_inner_region() {
        let child = Box::new(MockChild::new(vec!["hello"]));
        let mut ov = OverlayWindow::new("T", child, identity_theme(), 5);
        let lines = ov.render(20);
        let hello_row = lines
            .iter()
            .find(|l| l.contains("hello"))
            .expect("hello row");
        assert!(hello_row.starts_with("│ hello"), "row: {hello_row:?}");
        assert!(hello_row.ends_with('│'), "row ends with │: {hello_row:?}");
        assert_eq!(visible_width(hello_row), 20);
    }

    #[test]
    fn bottom_edge_present() {
        let child = Box::new(MockChild::new(vec!["x"]));
        let mut ov = OverlayWindow::new("T", child, identity_theme(), 5);
        let lines = ov.render(20);
        let bottom = lines.last().expect("has bottom");
        assert!(bottom.starts_with('╰'), "bottom: {bottom:?}");
        assert!(bottom.ends_with('╯'), "bottom: {bottom:?}");
        assert_eq!(visible_width(bottom), 20);
    }

    #[test]
    fn input_is_delegated_to_child() {
        let child = Box::new(MockChild::new(vec!["x"]));
        let mut ov = OverlayWindow::new("T", child, identity_theme(), 5);
        let event = InputEvent::Paste(String::new());
        assert!(ov.handle_input(&event));
        let mc = ov
            .child
            .as_any()
            .downcast_ref::<MockChild>()
            .expect("downcast");
        assert_eq!(mc.input_count, 1);
    }

    #[test]
    fn focus_is_delegated_to_child() {
        let child = Box::new(MockChild::new(vec!["x"]));
        let mut ov = OverlayWindow::new("T", child, identity_theme(), 5);
        assert!(!ov.is_focused());
        ov.set_focused(true);
        assert!(ov.is_focused());
        let mc = ov
            .child
            .as_any()
            .downcast_ref::<MockChild>()
            .expect("downcast");
        assert!(mc.focused);
    }

    #[test]
    fn renders_to_stable_inner_row_count_regardless_of_child_size() {
        let small_child = Box::new(MockChild::new(vec!["a"]));
        let mut ov = OverlayWindow::new("T", small_child, identity_theme(), 10);
        let small_render = ov.render(40);
        assert_eq!(small_render.len(), 14); // 10 inner + 4 chrome

        let larger_lines: Vec<&str> = vec!["a"; 8];
        let large_child = Box::new(MockChild::new(larger_lines));
        let mut ov = OverlayWindow::new("T", large_child, identity_theme(), 10);
        let large_render = ov.render(40);
        assert_eq!(large_render.len(), 14);
    }

    #[test]
    fn truncates_oversized_child() {
        let lines: Vec<&str> = vec!["a"; 20];
        let child = Box::new(MockChild::new(lines));
        let mut ov = OverlayWindow::new("T", child, identity_theme(), 5);
        let render = ov.render(40);
        assert_eq!(render.len(), 9); // 5 inner + 4 chrome
    }

    #[test]
    fn subtitle_renders_on_bottom_border() {
        let child = Box::new(MockChild::new(vec!["x"]));
        let mut ov = OverlayWindow::new("T", child, identity_theme(), 5).with_subtitle("hint");
        let r = ov.render(40);
        let bottom = r.last().unwrap();
        assert!(bottom.contains("hint"), "bottom: {bottom:?}");
        assert!(bottom.starts_with('╰'), "bottom: {bottom:?}");
        assert!(bottom.ends_with('╯'), "bottom: {bottom:?}");
        assert_eq!(visible_width(bottom), 40);
    }

    #[test]
    fn dynamic_subtitle_is_resolved_from_child_state_each_frame() {
        let child = Box::new(MockChild::new(vec!["x"]));
        let mut ov = OverlayWindow::new("T", child, identity_theme(), 5).with_dynamic_subtitle(
            |child: &dyn Component| {
                let mc = child.as_any().downcast_ref::<MockChild>().expect("mock");
                format!("inputs:{}", mc.input_count)
            },
        );
        let r = ov.render(40);
        assert!(r.last().unwrap().contains("inputs:0"));

        ov.handle_input(&InputEvent::Paste(String::new()));
        let r = ov.render(40);
        assert!(r.last().unwrap().contains("inputs:1"));
    }

    #[test]
    fn available_height_is_forwarded_to_child_before_render() {
        let child = Box::new(MockChild::new(vec!["x"]));
        let mut ov = OverlayWindow::new("T", child, identity_theme(), 7);
        ov.render(40);
        let mc = ov
            .child
            .as_any()
            .downcast_ref::<MockChild>()
            .expect("downcast");
        assert_eq!(mc.last_available_height, Some(7));
    }

    #[test]
    fn dynamic_height_tracks_terminal_rows() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU16, Ordering};

        use crate::tui::RenderHandle;

        // Policy: inner_rows = term_rows / 2. Drive `term_rows` via the
        // shared atomic the handle reads, re-render, and assert both the
        // total box height and the value pushed to the child track it.
        let rows = Arc::new(AtomicU16::new(20));
        let handle = RenderHandle::detached_with_shared_rows(Arc::clone(&rows));
        let child = Box::new(MockChild::new(vec!["a"]));
        let mut ov = OverlayWindow::new("T", child, identity_theme(), 5)
            .with_dynamic_height(handle, |term_rows| term_rows / 2);

        let tall = ov.render(40);
        // 20 rows -> 10 inner -> 14 total (10 inner + 4 chrome).
        assert_eq!(tall.len(), 14);
        let pushed = ov
            .child
            .as_any()
            .downcast_ref::<MockChild>()
            .expect("downcast")
            .last_available_height;
        assert_eq!(pushed, Some(10));

        rows.store(40, Ordering::Relaxed);
        let taller = ov.render(40);
        // 40 rows -> 20 inner -> 24 total.
        assert_eq!(taller.len(), 24);
        let pushed = ov
            .child
            .as_any()
            .downcast_ref::<MockChild>()
            .expect("downcast")
            .last_available_height;
        assert_eq!(pushed, Some(20));
    }
}
