//! The component model for the TUI framework.
//!
//! Components are the building blocks of the UI. Each component knows how to
//! render itself to a set of terminal lines and optionally handle input events.

use std::any::Any;
use std::fmt;
use std::ops::Deref;
use std::rc::Rc;

use crate::keys::InputEvent;

/// A single fully-rendered terminal row, shared cheaply across frames.
///
/// [`Component::render`] returns `Vec<Line>`. Cloning a `Line` bumps a
/// refcount instead of copying the row's bytes, so a component that
/// returns the same cached rows every frame hands the render engine the
/// *same* allocations each time. The engine leans on that: [`Line::same_alloc`]
/// compares pointer identity, letting the renderer detect an unchanged row
/// in O(1) and skip re-normalizing, re-diffing, and re-painting it. That is
/// what keeps per-frame cost proportional to what changed rather than to
/// the whole (potentially huge) scrollback.
///
/// `Line` wraps `Rc<str>`, not `Arc<str>`, so it is `!Send`. The render
/// loop is single-threaded (it runs on the main task via `block_on` and is
/// never `tokio::spawn`ed) and rendered rows never cross a thread boundary,
/// so the non-atomic refcount is sound. The `!Send` doubles as a guard: any
/// attempt to move the component tree onto another task fails to compile.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct Line(Rc<str>);

impl Line {
    /// Borrow the row's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// True if both lines point at the same allocation.
    ///
    /// A component that returns its cached rows unchanged hands back the
    /// same `Rc` each frame, so pointer identity is a sound, O(1) proxy
    /// for "this row did not change since the previous frame". Content
    /// equality (`==`) is the fallback for rows that were rebuilt to the
    /// same bytes.
    pub fn same_alloc(&self, other: &Line) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
    }
}

impl Default for Line {
    fn default() -> Self {
        Line(Rc::from(""))
    }
}

impl Deref for Line {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for Line {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl From<String> for Line {
    fn from(s: String) -> Self {
        Line(Rc::from(s))
    }
}

impl From<&str> for Line {
    fn from(s: &str) -> Self {
        Line(Rc::from(s))
    }
}

impl From<std::borrow::Cow<'_, str>> for Line {
    fn from(s: std::borrow::Cow<'_, str>) -> Self {
        Line(Rc::from(s))
    }
}

impl fmt::Debug for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl fmt::Display for Line {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// Comparisons against string slices/owned strings keep call sites and
// tests ergonomic (`line == "x"`, `assert_eq!(lines, vec!["a", "b"])`).
// Both directions are provided so either operand order works.
impl PartialEq<str> for Line {
    fn eq(&self, other: &str) -> bool {
        &*self.0 == other
    }
}

impl PartialEq<&str> for Line {
    fn eq(&self, other: &&str) -> bool {
        &*self.0 == *other
    }
}

impl PartialEq<String> for Line {
    fn eq(&self, other: &String) -> bool {
        &*self.0 == other.as_str()
    }
}

impl PartialEq<Line> for str {
    fn eq(&self, other: &Line) -> bool {
        self == &*other.0
    }
}

impl PartialEq<Line> for &str {
    fn eq(&self, other: &Line) -> bool {
        *self == &*other.0
    }
}

impl PartialEq<Line> for String {
    fn eq(&self, other: &Line) -> bool {
        self.as_str() == &*other.0
    }
}

/// A zero-width APC escape sequence used as a cursor position marker.
///
/// Components that implement `Focusable` embed this marker in their render output
/// at the cursor position. The TUI engine extracts it, strips it, and positions
/// the hardware cursor there (for IME support).
pub const CURSOR_MARKER: &str = "\x1b_tui:c\x07";

/// The core trait for all TUI components.
///
/// Components render themselves to lines of text that fit within a given width,
/// and optionally handle input events when focused.
pub trait Component: Any {
    /// Render the component to lines fitting within `width` visible columns.
    ///
    /// Each returned [`Line`] may contain ANSI escape codes for styling.
    /// The visible width of each line must not exceed `width`.
    ///
    /// Takes `&mut self` because rendering is allowed to mutate internal
    /// bookkeeping: scroll position, layout caches, drained async result
    /// channels, and so on. Components that don't need mutation can just
    /// ignore it.
    ///
    /// A component that caches its rows should cache `Vec<Line>` and return
    /// clones of it. Returning the same `Line` allocations across frames
    /// lets the engine recognize unchanged rows by pointer identity (see
    /// [`Line::same_alloc`]) and skip re-processing them.
    fn render(&mut self, width: usize) -> Vec<Line>;

    /// Handle an input event. Returns `true` if the event was consumed.
    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    /// Whether this component wants to receive key-release events.
    ///
    /// The Kitty keyboard protocol (and `crossterm` when
    /// `REPORT_EVENT_TYPES` is active) delivers both press and release
    /// events for every key. Most components only care about presses, so
    /// `Tui::handle_input` filters releases out before calling
    /// [`Component::handle_input`]. Components that explicitly want to
    /// observe a key being let go (chord detection, hold-to-repeat
    /// overrides, debug overlays) override this to return `true`.
    ///
    /// Key-repeat events (`KeyEventKind::Repeat`) are always delivered
    /// because they behave like additional presses; only releases are
    /// gated.
    fn wants_key_release(&self) -> bool {
        false
    }

    /// Clear any cached render state. Called when themes change, terminal
    /// resizes, or other external state invalidates the cached output.
    fn invalidate(&mut self) {}

    /// Inform the component of the inner content height (rows) it will be
    /// rendered into this frame, before [`Component::render`] is called.
    /// Containers that impose a height (e.g.
    /// [`OverlayWindow`][crate::components::overlay_window::OverlayWindow])
    /// call this so the child can size internal scroll regions to match.
    /// Default: ignore.
    fn set_available_height(&mut self, _rows: usize) {}

    /// Notify the component that its keyboard focus state changed.
    /// Components that care about focus (text inputs, editors, lists)
    /// should override this to toggle cursor visibility, input routing,
    /// or any other focus-dependent state. The default is a no-op.
    fn set_focused(&mut self, _focused: bool) {}

    /// Returns whether this component currently has keyboard focus.
    /// The default returns `false`; focus-aware components should
    /// override to track their own state.
    fn is_focused(&self) -> bool {
        false
    }

    /// Downcast to `Any` for type-safe access to concrete component types.
    fn as_any(&self) -> &dyn Any;

    /// Downcast to `Any` for type-safe mutable access to concrete component types.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// Macro to implement the `as_any` and `as_any_mut` methods for a component.
#[macro_export]
macro_rules! impl_component_any {
    () => {
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
    };
}
