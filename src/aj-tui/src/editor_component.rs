//! Pluggable editor abstraction.
//!
//! The agent layer sometimes wants to swap in its own editor — vim
//! mode, emacs mode, an extension-supplied widget — while still
//! routing input through the same TUI plumbing as the built-in
//! [`crate::components::editor::Editor`] and
//! [`crate::components::text_input::TextInput`]. The
//! [`EditorComponent`] trait is the contract those alternatives
//! implement so the host code can talk to any editor uniformly.
//!
//! Input flows through [`crate::component::Component::handle_input`]
//! as typed [`crate::keys::InputEvent`] values; the byte-level parsing
//! happens one layer down (crossterm's `EventStream`), so the trait
//! does not carry a raw-bytes entry point. Hosts that need to inject
//! a paste programmatically push an [`crate::keys::InputEvent::Paste`]
//! through the same dispatch path the terminal would use.

use std::sync::Arc;

use crate::autocomplete::AutocompleteProvider;
use crate::component::Component;

/// A text-editing component the agent layer can plug in interchangeably.
///
/// Required methods cover the minimum surface the host needs to read
/// the user's text, replace it programmatically, and observe submit /
/// change events. Optional methods (history, cursor-relative insert,
/// expanded text, autocomplete, padding) all default to a no-op or
/// the obvious fallback so simple editors only need to implement the
/// required core.
///
/// The trait is object-safe: the host stores editors as
/// `Box<dyn EditorComponent>` and dispatches dynamically. All methods
/// take `&self` / `&mut self` and use only `dyn`-friendly parameter
/// types.
pub trait EditorComponent: Component {
    // --- Required ---

    /// Current text content. For multi-line editors this is the
    /// document with logical lines joined by `\n`.
    fn text(&self) -> String;

    /// Replace the entire text content. Cursor handling is
    /// implementation-defined; built-in editors move the cursor to
    /// the end of the new text.
    fn set_text(&mut self, text: &str);

    /// Install the submit callback, fired when the user confirms the
    /// current value (e.g. plain Enter on a single-line input,
    /// configurable submit binding on a multi-line editor). Replaces
    /// any previously-installed callback.
    fn set_on_submit(&mut self, callback: Box<dyn FnMut(&str)>);

    /// Install the change callback, fired whenever the text content
    /// changes through user input or programmatic mutation. Replaces
    /// any previously-installed callback.
    fn set_on_change(&mut self, callback: Box<dyn FnMut(&str)>);

    // --- Optional, default no-op ---

    /// Push `text` onto the editor's history ring (for up/down
    /// navigation). Editors without history ignore the call.
    fn add_to_history(&mut self, _text: &str) {}

    /// Insert `text` at the current cursor position. Editors that
    /// don't model a cursor position (or don't expose programmatic
    /// insertion) ignore the call. Built-in editors treat the whole
    /// insertion as a single undo unit.
    fn insert_text_at_cursor(&mut self, _text: &str) {}

    /// Install an autocomplete provider. Editors without an
    /// autocomplete pipeline ignore the call.
    fn set_autocomplete_provider(&mut self, _provider: Arc<dyn AutocompleteProvider>) {}

    /// Set the editor's horizontal padding, in columns. Editors that
    /// don't support inner padding ignore the call.
    fn set_padding_x(&mut self, _padding: usize) {}

    /// Cap the maximum number of suggestions visible in the
    /// autocomplete popup. Editors without an autocomplete popup
    /// ignore the call.
    fn set_autocomplete_max_visible(&mut self, _max: usize) {}

    /// The editor's border-color closure, if it has one.
    ///
    /// Returns `None` for editors that don't render a border (e.g. the
    /// single-line [`crate::components::text_input::TextInput`]). Hosts can
    /// branch on the `Option` to decide whether to attempt a swap via
    /// [`Self::set_border_color`].
    ///
    /// The default implementation returns `None`, so simple editors
    /// without a border-color concept don't need to override.
    fn border_color(&self) -> Option<std::sync::Arc<dyn Fn(&str) -> String>> {
        None
    }

    /// Replace the editor's border-color closure.
    ///
    /// Hosts that swap UI modes (e.g. a "thinking" mode that tints the
    /// editor border, or a bash-mode toggle) call this to update the
    /// border without having to rebuild the editor. Editors that don't
    /// render a border ignore the call (the default no-op impl).
    fn set_border_color(&mut self, _color: std::sync::Arc<dyn Fn(&str) -> String>) {}

    // --- Optional, with non-trivial default ---

    /// Text content with any internal markers expanded. The default
    /// returns [`Self::text`] verbatim; editors that hide bulk pastes
    /// behind placeholder tokens override this to splice the
    /// placeholders back to their original content.
    fn expanded_text(&self) -> String {
        self.text()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::components::editor::{Editor, EditorTheme};
    use crate::components::select_list::SelectListTheme;
    use crate::components::text_input::TextInput;
    use crate::tui::RenderHandle;

    fn theme() -> EditorTheme {
        EditorTheme {
            border_color: Arc::new(|s| s.to_string()),
            select_list: SelectListTheme {
                selected_prefix: Arc::new(|s| s.to_string()),
                selected_text: Arc::new(|s| s.to_string()),
                description: Arc::new(|s| s.to_string()),
                scroll_info: Arc::new(|s| s.to_string()),
                no_match: Arc::new(|s| s.to_string()),
            },
        }
    }

    /// Round-trip both built-in editors through `Box<dyn EditorComponent>`
    /// to confirm object safety and that the required + optional
    /// methods route correctly through dynamic dispatch.
    #[test]
    fn box_dyn_editor_component_round_trip() {
        let editor: Box<dyn EditorComponent> =
            Box::new(Editor::new(RenderHandle::detached(), theme()));
        let input: Box<dyn EditorComponent> = Box::new(TextInput::new("> "));

        for mut ed in [editor, input] {
            // set/get round-trip.
            ed.set_text("hello");
            assert_eq!(ed.text(), "hello");

            // Cursor-relative insert: `Editor` overrides the optional
            // method and appends "X" after the existing text (the
            // cursor sits at end-of-text after `set_text`). `TextInput`
            // doesn't override it and inherits the no-op default — it's
            // single-line, has no cursor-relative-insert concept, and
            // would route bulk insertion through `set_text` instead.
            // Assert only that the call doesn't panic and that the
            // original text survives, which holds for both branches.
            let before = ed.text();
            ed.insert_text_at_cursor("X");
            let after = ed.text();
            assert!(after.contains("hello"));
            assert!(after.len() >= before.len());

            // Expanded text falls back to `text` for editors without
            // paste markers.
            assert_eq!(ed.expanded_text(), ed.text());
        }
    }

    /// Submit callback fires through the trait setter. Verifies that
    /// `set_on_submit` is wired into the existing `on_submit` field
    /// path and not a parallel slot.
    #[test]
    fn set_on_submit_wires_into_existing_callback_path() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_clone = Arc::clone(&calls);

        let mut input = TextInput::new("> ");
        let input_dyn: &mut dyn EditorComponent = &mut input;
        input_dyn.set_on_submit(Box::new(move |text| {
            assert_eq!(text, "submitted");
            calls_clone.fetch_add(1, Ordering::SeqCst);
        }));

        // Simulate a submit by calling the underlying field directly —
        // we're verifying the setter installs the callback, not the
        // dispatch path (already covered by component-level tests).
        if let Some(ref mut cb) = input.on_submit {
            cb("submitted");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// `border_color()` returns the current closure for editors that
    /// render a border (the multi-line `Editor`), and `None` for
    /// editors that don't (the single-line `TextInput`). Hosts use the
    /// `Option` to gate the matching `set_border_color` call.
    #[test]
    fn border_color_getter_reflects_per_editor_capability() {
        let editor: Box<dyn EditorComponent> =
            Box::new(Editor::new(RenderHandle::detached(), theme()));
        let input: Box<dyn EditorComponent> = Box::new(TextInput::new("> "));

        // The default `theme()` fixture installs an identity closure on
        // `border_color`, so the getter returns `Some(closure)` and the
        // closure round-trips a sample byte-for-byte.
        let editor_color = editor.border_color().expect("Editor has a border color");
        assert_eq!(editor_color("─"), "─");

        // `TextInput` doesn't render a border — the trait default returns
        // `None`, which is the host's signal to skip the swap.
        assert!(
            input.border_color().is_none(),
            "TextInput has no border-color concept"
        );
    }

    /// `set_border_color()` swaps the closure on a `Box<dyn EditorComponent>`
    /// and the next read sees the new closure. Mirrors the host pattern
    /// of toggling editor tint when the UI mode changes.
    #[test]
    fn set_border_color_swaps_the_closure_through_dyn_dispatch() {
        let mut editor: Box<dyn EditorComponent> =
            Box::new(Editor::new(RenderHandle::detached(), theme()));

        // Sanity: the original closure echoes its input.
        let initial = editor.border_color().expect("editor has a border color");
        assert_eq!(initial("test"), "test");
        // Drop the borrow before mutating `editor`: `border_color()`
        // returns an owned `Arc`, but holding it across `set_border_color`
        // would prevent the trait method from running.
        drop(initial);

        // Swap to a wrapping closure and verify the read picks it up.
        editor.set_border_color(Arc::new(|s: &str| format!("[{}]", s)));
        let updated = editor
            .border_color()
            .expect("editor still has a border color after swap");
        assert_eq!(updated("test"), "[test]");
    }

    /// `set_border_color` on an editor without a border concept must
    /// be a no-op rather than a panic — the trait advertises an
    /// optional capability and `TextInput` opts out by accepting the
    /// default body.
    #[test]
    fn set_border_color_on_input_is_a_silent_noop() {
        let mut input = TextInput::new("> ");
        let input_dyn: &mut dyn EditorComponent = &mut input;
        // Should not panic.
        input_dyn.set_border_color(Arc::new(|s: &str| format!("[{}]", s)));
        // Getter still returns `None` because the default impl ignores
        // the swap.
        assert!(input_dyn.border_color().is_none());
    }
}
