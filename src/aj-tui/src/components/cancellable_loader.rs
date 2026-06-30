//! Loader that can be cancelled with Escape or Ctrl+C.
//!
//! Wraps [`Loader`] with a [`CancellationToken`] for cancelling
//! async operations. The token is the idiomatic Rust analog of an
//! abort signal — clonable, observable, and integrated with the
//! async ecosystem via `tokio::select!` on `cancelled().await`.
//!
//! Typical usage:
//!
//! ```ignore
//! let mut loader = CancellableLoader::new(
//!     tui.handle(),
//!     Box::new(style::cyan),
//!     Box::new(style::dim),
//!     "Working...",
//! );
//! let cancel = loader.cancel_token();
//! tui.add_child(Box::new(loader));
//!
//! // Somewhere in your async work:
//! tokio::select! {
//!     _ = cancel.cancelled() => { /* user cancelled */ }
//!     result = do_work() => { /* completed */ }
//! }
//! ```
//!
//! When the user presses any key bound to `tui.select.cancel` while
//! the loader has focus (Escape or Ctrl+C by default), the cancel
//! token flips to cancelled and any `on_abort` callback registered
//! on the loader fires.

use tokio_util::sync::CancellationToken;

use crate::component::{Component, Line};
use crate::components::loader::Loader;
use crate::keybindings;
use crate::keys::InputEvent;
use crate::tui::RenderHandle;

/// A [`Loader`] that cancels a [`CancellationToken`] (and optionally
/// fires a callback) when the user presses a cancel key.
pub struct CancellableLoader {
    inner: Loader,
    cancel: CancellationToken,
    on_abort: Option<Box<dyn FnMut()>>,
    focused: bool,
}

impl CancellableLoader {
    /// Create a cancellable loader with the given styling closures and
    /// message.
    ///
    /// Same shape as [`Loader::new`]; see that constructor for the
    /// full rationale. Use [`CancellableLoader::with_identity_styles`]
    /// for the common test/no-styling case.
    ///
    /// `handle` is forwarded to the wrapped [`Loader`] for animation
    /// pumping. See [`Loader::new`] for the handle's role; standalone
    /// callers pass [`RenderHandle::detached`].
    pub fn new(
        handle: RenderHandle,
        spinner_style: Box<dyn Fn(&str) -> String>,
        message_style: Box<dyn Fn(&str) -> String>,
        message: &str,
    ) -> Self {
        Self {
            inner: Loader::new(handle, spinner_style, message_style, message),
            cancel: CancellationToken::new(),
            on_abort: None,
            focused: false,
        }
    }

    /// Construct a cancellable loader with identity styling closures.
    /// Convenience for tests and other callers that don't need themed
    /// output. See [`Loader::with_identity_styles`].
    pub fn with_identity_styles(handle: RenderHandle, message: &str) -> Self {
        Self::new(
            handle,
            Box::new(|s| s.to_string()),
            Box::new(|s| s.to_string()),
            message,
        )
    }

    /// Replace the message.
    pub fn set_message(&mut self, message: &str) {
        self.inner.set_message(message);
    }

    /// Register a callback to run when the user presses a cancel key
    /// (`tui.select.cancel`, default `escape` or `ctrl+c`). Fires on
    /// every cancel-key press while the loader is focused — subsequent
    /// presses on an already-cancelled loader still fire the
    /// callback. Cancellation itself is idempotent (the
    /// [`CancellationToken`] only flips false → true once), so most
    /// `on_abort` callbacks should be written to be re-entrancy-safe.
    pub fn set_on_abort(&mut self, cb: Box<dyn FnMut()>) {
        self.on_abort = Some(cb);
    }

    /// [`CancellationToken`] that is cancelled when the user presses a
    /// cancel key. Workers can poll via
    /// [`CancellationToken::is_cancelled`] or await via
    /// [`CancellationToken::cancelled`] / `tokio::select!`. Cheap to
    /// clone; clones share the cancellation state.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Whether cancel has been requested.
    pub fn is_aborted(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Stop the underlying spinner without tripping cancellation.
    pub fn stop(&mut self) {
        self.inner.stop();
    }
}

impl Component for CancellableLoader {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<Line> {
        self.inner.render(width)
    }

    /// Forward invalidation to the wrapped [`Loader`] so its embedded
    /// [`Text`] body's render cache clears. The framework calls this
    /// on global events (terminal resize, theme palette swap) that can
    /// change rendered output independently of the loader's own
    /// state. Without forwarding, the body's cache would survive
    /// until the next message or frame change.
    fn invalidate(&mut self) {
        self.inner.invalidate();
    }

    /// On a cancel-key match, cancel the token and fire the
    /// `on_abort` callback. The spinner is *not* auto-stopped —
    /// that's left to the parent, which typically calls
    /// [`CancellableLoader::stop`] from inside its `on_abort`
    /// handler. When the loader is removed from the tree,
    /// `Drop for Loader` cancels the animation pump regardless.
    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.cancel") {
            self.cancel.cancel();
            if let Some(ref mut cb) = self.on_abort {
                cb();
            }
            return true;
        }
        false
    }

    fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    fn is_focused(&self) -> bool {
        self.focused
    }
}
