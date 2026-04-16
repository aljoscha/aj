//! Loader that can be cancelled with Escape or Ctrl+C.
//!
//! Wraps [`Loader`] with an abort signal so an async operation can
//! observe when the user asks to cancel. The cancel flag is a shared
//! [`Arc<AtomicBool>`] that any worker can poll without coupling to a
//! specific runtime's cancellation primitive.
//!
//! Typical usage:
//!
//! ```ignore
//! let mut loader = CancellableLoader::new("Working...");
//! loader.set_spinner_style(Box::new(style::cyan));
//! let cancel = loader.cancel_flag();
//! tui.root.add_child(Box::new(loader));
//!
//! // Somewhere in your async work:
//! while !cancel.load(Ordering::SeqCst) {
//!     // ...do a chunk of work...
//! }
//! ```
//!
//! When the user presses any key bound to `tui.select.cancel` while
//! the loader has focus (Escape or Ctrl+C by default), the cancel
//! flag flips to `true` and any `on_abort` callback registered on the
//! loader fires.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::component::Component;
use crate::components::loader::Loader;
use crate::keybindings;
use crate::keys::InputEvent;

/// A [`Loader`] that flips an abort flag (and optionally fires a
/// callback) when the user presses Escape.
pub struct CancellableLoader {
    inner: Loader,
    cancel: Arc<AtomicBool>,
    on_abort: Option<Box<dyn FnMut()>>,
    focused: bool,
}

impl CancellableLoader {
    /// Create a cancellable loader with the given message.
    pub fn new(message: &str) -> Self {
        Self {
            inner: Loader::new(message),
            cancel: Arc::new(AtomicBool::new(false)),
            on_abort: None,
            focused: false,
        }
    }

    /// Replace the spinner style. Delegates to the wrapped [`Loader`].
    pub fn set_spinner_style(&mut self, style_fn: Box<dyn Fn(&str) -> String>) {
        self.inner.set_spinner_style(style_fn);
    }

    /// Replace the message style. Delegates to the wrapped [`Loader`].
    pub fn set_message_style(&mut self, style_fn: Box<dyn Fn(&str) -> String>) {
        self.inner.set_message_style(style_fn);
    }

    /// Replace the message.
    pub fn set_message(&mut self, message: &str) {
        self.inner.set_message(message);
    }

    /// Register a callback to run when the user presses a cancel key
    /// (`tui.select.cancel`, default `escape` or `ctrl+c`). Fires once
    /// and only while the loader is focused.
    pub fn set_on_abort(&mut self, cb: Box<dyn FnMut()>) {
        self.on_abort = Some(cb);
    }

    /// Shared handle to the cancel flag. Clone this into the async
    /// worker that should respect the request.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    /// Whether cancel has been requested.
    pub fn is_aborted(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// Manually trip the cancel flag, the same way a cancel key would.
    /// The `on_abort` callback does NOT fire on a manual abort — callers
    /// that want that behavior can invoke it themselves. This makes
    /// it safe to call from code paths that need to tear down the
    /// loader without re-entering the callback.
    pub fn abort(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.inner.stop();
    }

    /// Stop the underlying spinner without tripping the cancel flag.
    pub fn stop(&mut self) {
        self.inner.stop();
    }
}

impl Component for CancellableLoader {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        self.inner.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();
        if kb.matches(event, "tui.select.cancel") {
            if !self.cancel.swap(true, Ordering::SeqCst) {
                // Transitioned from false to true: fire the
                // callback exactly once.
                if let Some(ref mut cb) = self.on_abort {
                    cb();
                }
            }
            self.inner.stop();
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
