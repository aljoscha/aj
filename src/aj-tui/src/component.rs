//! The component model for the TUI framework.
//!
//! Components are the building blocks of the UI. Each component knows how to
//! render itself to a set of terminal lines and optionally handle input events.

use std::any::Any;

use crate::keys::InputEvent;

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
    /// Each returned string may contain ANSI escape codes for styling.
    /// The visible width of each line must not exceed `width`.
    ///
    /// Takes `&mut self` because rendering is allowed to mutate internal
    /// bookkeeping: scroll position, layout caches, drained async result
    /// channels, and so on. Components that don't need mutation can just
    /// ignore it.
    fn render(&mut self, width: usize) -> Vec<String>;

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

    /// Receive a [`crate::tui::RenderHandle`] from the owning [`crate::tui::Tui`].
    ///
    /// Called once by the `Tui` when the component enters the tree
    /// (via [`crate::container::Container::add_child`],
    /// [`crate::container::Container::insert_child`], or
    /// [`crate::tui::Tui::show_overlay`]). Components that spawn async
    /// work which should schedule a repaint when it finishes — most
    /// notably [`crate::components::editor::Editor`] and its
    /// autocomplete pipeline — store a clone of the handle here.
    /// Components with no async needs can leave the default no-op
    /// alone.
    ///
    /// Containers should forward to their children.
    fn set_render_handle(&mut self, _handle: crate::tui::RenderHandle) {}

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
