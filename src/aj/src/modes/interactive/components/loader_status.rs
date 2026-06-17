//! Loader / status indicator wrapper.
//!
//! Renders a spinner + message while the agent is mid-turn (between
//! `AgentStart` and `AgentEnd`, or while a tool is running). The
//! event pump owns one instance and toggles its visibility through
//! [`LoaderStatus::start`] / [`LoaderStatus::stop`]; the surrounding
//! `status` slot in the layout owns the `Box<dyn Component>` so the
//! loader can be added to / removed from the rendered tree without
//! the pump having to track raw pointers.
//!
//! The message defaults to "Working…" but the pump relabels it via
//! [`LoaderStatus::set_message`] for non-turn activity — today, the
//! per-phase status of an in-flight compaction — and restores the
//! default with [`LoaderStatus::reset_message`] when that activity
//! ends.
//!
//! See `docs/aj-next-plan.md` §4 — `components/loader_status.rs`.

use std::any::Any;

use aj_tui::component::Component;
use aj_tui::components::loader::Loader;
use aj_tui::keys::InputEvent;
use aj_tui::style;
use aj_tui::tui::RenderHandle;

/// Default working message — kept in sync with the legacy CLI's
/// `display_loader` so users don't see a different status word
/// between the two binaries during the Phase 0 → Phase 2 window.
/// The "(Ctrl+C to cancel)" suffix surfaces the cancellation
/// affordance per `docs/aj-next-plan.md` §1.8 so users can discover
/// it without consulting docs.
pub fn default_message() -> String {
    format!(
        "Working… ({} to cancel)",
        crate::config::keybindings::fixed_keys::CTRL_C
    )
}

/// Component wrapping an [`aj_tui::components::loader::Loader`]
/// with a small set of agent-aware affordances:
///
/// - Construction takes a [`RenderHandle`] so the loader can wake
///   the TUI's render throttle when its frame advances; without
///   this the spinner would freeze between input events.
/// - The wrapper renders an empty line list when the loader is
///   stopped, so attaching it permanently to the `status` slot
///   is fine — an idle agent shows nothing, a running agent
///   shows the spinner.
pub struct LoaderStatus {
    loader: Loader,
    /// Whether the loader is currently active. The inner
    /// [`Loader`] tracks this independently, but mirroring it on
    /// the wrapper keeps `render` cheap and self-contained.
    active: bool,
}

impl LoaderStatus {
    /// Build a fresh, **inactive** loader status. The component
    /// renders nothing until [`Self::start`] is called.
    pub fn new(handle: RenderHandle) -> Self {
        let mut loader = Loader::new(
            handle,
            // Cyan spinner against the default-coloured message
            // matches the rest of the palette (assistant headers,
            // user message prefixes, list bullets are all cyan).
            Box::new(style::cyan),
            // Dim message text so the spinner draws the eye
            // rather than the word.
            Box::new(style::dim),
            &default_message(),
        );
        // `Loader::new` starts the animation pump immediately.
        // We want a quiet idle state, so stop it right away; the
        // event pump calls `start()` when the agent begins a turn.
        loader.stop();
        Self {
            loader,
            active: false,
        }
    }

    /// Begin animating the spinner with the current message.
    pub fn start(&mut self) {
        self.loader.start();
        self.active = true;
    }

    /// Stop animating and hide the spinner.
    pub fn stop(&mut self) {
        self.loader.stop();
        self.active = false;
    }

    /// Replace the displayed status message. Triggers an
    /// immediate repaint via the underlying loader.
    pub fn set_message(&mut self, message: &str) {
        self.loader.set_message(message);
    }

    /// Reset the message to [`default_message`].
    pub fn reset_message(&mut self) {
        self.set_message(&default_message());
    }

    /// Whether the loader is currently animating.
    pub fn is_active(&self) -> bool {
        self.active
    }
}

impl Component for LoaderStatus {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        if !self.active {
            // Idle agent → no rendered rows. The `status` slot
            // collapses to zero height between turns so the chat
            // scrollback sits flush against the editor.
            return Vec::new();
        }
        self.loader.render(width)
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        self.loader.invalidate();
    }
}

impl AsRef<dyn Any> for LoaderStatus {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_loader_is_inactive_and_renders_nothing() {
        let mut s = LoaderStatus::new(RenderHandle::detached());
        assert!(!s.is_active());
        assert!(s.render(80).is_empty());
    }

    #[test]
    fn start_sets_active_and_renders_at_least_one_line() {
        let mut s = LoaderStatus::new(RenderHandle::detached());
        s.start();
        assert!(s.is_active());
        let lines = s.render(80);
        assert!(!lines.is_empty(), "active loader should produce a row");
    }

    #[test]
    fn stop_clears_active_state() {
        let mut s = LoaderStatus::new(RenderHandle::detached());
        s.start();
        s.stop();
        assert!(!s.is_active());
        assert!(s.render(80).is_empty());
    }
}
