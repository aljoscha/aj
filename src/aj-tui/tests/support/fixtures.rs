//! Reusable component fixtures for tests.
//!
//! These are minimal components with no behavior of their own beyond what
//! a test needs to exercise the framework: render a fixed set of lines,
//! record input events, etc. Kept together so tests can reach for one
//! without copy-pasting the same four-line `impl Component` block.

use std::cell::RefCell;
use std::rc::Rc;

use aj_tui::component::Component;
use aj_tui::impl_component_any;
use aj_tui::keys::InputEvent;

/// A component that renders a fixed list of lines verbatim, one per row.
///
/// Useful as filler content for tests that aren't about rendering logic —
/// layout, overlay composition, input routing, etc. — where the component
/// just needs to put *something* on screen.
pub struct StaticLines {
    pub lines: Vec<String>,
}

impl StaticLines {
    /// Construct from anything that yields strings.
    pub fn new<I, S>(lines: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            lines: lines.into_iter().map(Into::into).collect(),
        }
    }
}

impl Component for StaticLines {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        self.lines.clone()
    }
}

/// A component that renders a list of lines which the test can mutate
/// after handing the component to the `Tui`.
///
/// The backing storage lives behind an `Rc<RefCell<_>>`, so cloning this
/// handle hands out another reference to the same line buffer: mutations
/// through any clone are visible on the next render pass. Use this for
/// streaming / incremental-content tests where the component is installed
/// once and the test repeatedly calls `set(...)` / `append(...)` between
/// `render_now` calls.
///
/// For tests whose content is fixed at construction time, prefer
/// [`StaticLines`] — it's a plain owning value with no interior
/// mutability cost.
///
/// ```ignore
/// let lines = MutableLines::new();
/// tui.add_child(Box::new(lines.clone()));
/// lines.set(["hello"]);
/// render_now(&mut tui);
/// assert_eq!(terminal.viewport()[0], "hello");
/// lines.append(["world"]);
/// render_now(&mut tui);
/// assert_eq!(terminal.viewport()[1], "world");
/// ```
#[derive(Clone, Default)]
pub struct MutableLines {
    lines: Rc<RefCell<Vec<String>>>,
}

impl MutableLines {
    /// Create an empty `MutableLines`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a `MutableLines` seeded with an initial set of lines.
    pub fn with_lines<I, S>(iter: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            lines: Rc::new(RefCell::new(iter.into_iter().map(Into::into).collect())),
        }
    }

    /// Replace the rendered lines.
    pub fn set<I, S>(&self, iter: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        *self.lines.borrow_mut() = iter.into_iter().map(Into::into).collect();
    }

    /// Append to the rendered lines.
    pub fn append<I, S>(&self, iter: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.lines
            .borrow_mut()
            .extend(iter.into_iter().map(Into::into));
    }

    /// Append a single line.
    pub fn push<S: Into<String>>(&self, line: S) {
        self.lines.borrow_mut().push(line.into());
    }

    /// Remove every line.
    pub fn clear(&self) {
        self.lines.borrow_mut().clear();
    }

    /// Number of rendered lines.
    pub fn len(&self) -> usize {
        self.lines.borrow().len()
    }

    /// Whether there are any rendered lines.
    pub fn is_empty(&self) -> bool {
        self.lines.borrow().is_empty()
    }

    /// Snapshot the current lines. Rarely needed in tests; the viewport
    /// read-back surfaces on [`super::virtual_terminal::VirtualTerminal`]
    /// are usually what you want.
    pub fn snapshot(&self) -> Vec<String> {
        self.lines.borrow().clone()
    }
}

impl Component for MutableLines {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        self.lines.borrow().clone()
    }
}

/// A component that records every `InputEvent` it receives and reports it
/// as handled.
///
/// The recorded log is held behind an `Rc<RefCell<_>>` so a test can keep a
/// handle to it after the component has been moved into the `Tui`.
pub struct InputRecorder {
    pub events: Rc<RefCell<Vec<InputEvent>>>,
}

impl InputRecorder {
    /// Create a recorder and return a shared handle to its event log.
    ///
    /// ```ignore
    /// let (recorder, events) = InputRecorder::new();
    /// tui.add_child(Box::new(recorder));
    /// tui.set_focus(Some(0));
    /// // ...send input...
    /// assert_eq!(events.borrow().len(), 1);
    /// ```
    pub fn new() -> (Self, Rc<RefCell<Vec<InputEvent>>>) {
        let events = Rc::new(RefCell::new(Vec::new()));
        let recorder = Self {
            events: Rc::clone(&events),
        };
        (recorder, events)
    }
}

impl Component for InputRecorder {
    impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        Vec::new()
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        self.events.borrow_mut().push(event.clone());
        true
    }
}

/// A component that renders a fixed list of lines verbatim and records
/// the `width` it was last rendered at.
///
/// Useful for overlay tests where the assertion is "the overlay was asked
/// to render at this exact width" — i.e. verifying the layout logic sized
/// the overlay correctly before the compositor got to it. The recorded
/// width lives behind an `Rc<RefCell<_>>` so the test can read it back
/// after handing the component to the `Tui`:
///
/// ```ignore
/// let (overlay, recorded) = StaticOverlay::new(["PCT"]);
/// tui.add_overlay(Box::new(overlay), OverlayOptions::default());
/// render_now(&mut tui);
/// assert_eq!(*recorded.borrow(), Some(40));
/// ```
///
/// Prefer [`StaticLines`] when the test is only about what landed on
/// screen and not about the width the component was called with.
pub struct StaticOverlay {
    lines: Vec<String>,
    rendered_width: Rc<RefCell<Option<usize>>>,
}

impl StaticOverlay {
    /// Construct an overlay from a set of lines and return both the
    /// overlay and a shared handle to the last-rendered-width slot.
    pub fn new<I, S>(lines: I) -> (Self, Rc<RefCell<Option<usize>>>)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let rendered_width = Rc::new(RefCell::new(None));
        let overlay = Self {
            lines: lines.into_iter().map(Into::into).collect(),
            rendered_width: Rc::clone(&rendered_width),
        };
        (overlay, rendered_width)
    }
}

impl Component for StaticOverlay {
    impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        *self.rendered_width.borrow_mut() = Some(width);
        self.lines.clone()
    }
}
