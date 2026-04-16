//! Loader that can be cancelled with Escape or Ctrl+C.
//!
//! Wraps [`Loader`] with a [`CancellationToken`] for cancelling
//! async operations. Mirrors pi-tui's
//! [`CancellableLoader`][pi-cancellable-loader] byte-for-byte:
//! pi's class extends [`Loader`] with an [`AbortController`] /
//! [`AbortSignal`] pair; the Rust port uses `tokio_util`'s
//! [`CancellationToken`], which is the idiomatic Rust analog
//! (clonable, observable, and integrated with the async ecosystem
//! via `tokio::select!` on `cancelled().await`).
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
//!
//! [pi-cancellable-loader]: https://github.com/pi-org/pi-mono/blob/main/packages/tui/src/components/cancellable-loader.ts
//! [`AbortController`]: https://developer.mozilla.org/en-US/docs/Web/API/AbortController
//! [`AbortSignal`]: https://developer.mozilla.org/en-US/docs/Web/API/AbortSignal

use tokio_util::sync::CancellationToken;

use crate::component::Component;
use crate::components::loader::Loader;
use crate::keybindings;
use crate::keys::InputEvent;
use crate::tui::RenderHandle;

/// A [`Loader`] that cancels a [`CancellationToken`] (and optionally
/// fires a callback) when the user presses a cancel key.
///
/// Mirrors pi-tui's `CancellableLoader extends Loader`:
/// [`cancel_token`][Self::cancel_token] is the analog of pi's
/// `signal: AbortSignal` getter, [`is_aborted`][Self::is_aborted] of
/// pi's `aborted: boolean`, and [`set_on_abort`][Self::set_on_abort]
/// of pi's `onAbort?: () => void` property.
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
    /// Mirrors [`Loader::new`]'s required-at-construction shape; see
    /// that constructor for the full rationale. Use
    /// [`CancellableLoader::with_identity_styles`] for the common
    /// test/no-styling case.
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
    /// (`tui.select.cancel`, default `escape` or `ctrl+c`). Mirrors
    /// pi-tui's `onAbort?: () => void` property: fires on every
    /// cancel-key press while the loader is focused (subsequent
    /// presses on an already-cancelled loader still fire the
    /// callback). Cancellation itself is idempotent — the
    /// [`CancellationToken`] only flips false → true once — so most
    /// `on_abort` callbacks should be written to be re-entrancy-safe.
    pub fn set_on_abort(&mut self, cb: Box<dyn FnMut()>) {
        self.on_abort = Some(cb);
    }

    /// [`CancellationToken`] that is cancelled when the user presses a
    /// cancel key. Mirrors pi-tui's `signal: AbortSignal` getter.
    /// Workers can poll via [`CancellationToken::is_cancelled`] or
    /// await via [`CancellationToken::cancelled`] / `tokio::select!`.
    /// Cheap to clone; clones share the cancellation state.
    pub fn cancel_token(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// Whether cancel has been requested. Equivalent to pi-tui's
    /// `aborted: boolean` getter; named with the `is_` prefix for
    /// Rust convention.
    pub fn is_aborted(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Stop the underlying spinner without tripping cancellation.
    /// Equivalent to pi-tui's `dispose()`, which itself just delegates
    /// to `this.stop()`.
    pub fn stop(&mut self) {
        self.inner.stop();
    }
}

impl Component for CancellableLoader {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }

    /// Mirrors pi-tui's `handleInput`:
    /// `if matches(cancel) { abortController.abort(); onAbort?.(); }`.
    /// The spinner is *not* auto-stopped — pi leaves that to the
    /// parent (which typically calls `dispose()` from inside its
    /// `onAbort` handler). When the loader is removed from the tree,
    /// `Drop for Loader` cancels the animation pump.
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
