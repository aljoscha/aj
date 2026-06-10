//! Session-wide render settings shared across chat components.
//!
//! The interactive transcript has a handful of global "how should
//! everything render" toggles — whether tool bodies show in full or
//! compact form, whether assistant thinking blocks are folded to a
//! placeholder, and whether tool image attachments render inline.
//! Every [`AssistantMessageComponent`] and [`ToolExecutionComponent`]
//! in the transcript (and inside every sub-agent box) has to honour
//! the same values.
//!
//! Rather than fan a toggle out to every existing component, the
//! values live behind a shared handle. Components hold a clone of the
//! handle and read it at render time; a flip through any clone is
//! visible to all of them. A monotonic [`generation`](RenderSettings::generation)
//! counter, bumped only when a value actually changes, lets a
//! component cheaply detect "something changed since I last rendered"
//! and rebuild just the derived caches it owns — without the toggle
//! site having to walk the transcript.
//!
//! The handle is `Rc` + `Cell` and therefore `!Send` / `!Sync`. That
//! matches the single-threaded TUI render loop, which owns every
//! component; no locking or atomics are required. Per-component render
//! state that is *not* session-wide (e.g. a sub-agent tool's
//! `header_only` flag, which tracks its box's mode) stays on the
//! component and is intentionally not modelled here.
//!
//! [`AssistantMessageComponent`]: crate::modes::interactive::components::assistant_message::AssistantMessageComponent
//! [`ToolExecutionComponent`]: crate::modes::interactive::components::tool_execution::ToolExecutionComponent

use std::cell::Cell;
use std::rc::Rc;

/// Shared, runtime-toggleable render settings for the chat
/// transcript. Cloning is a refcount bump; all clones observe the
/// same underlying values.
#[derive(Clone)]
pub struct RenderSettings(Rc<Inner>);

struct Inner {
    /// Render tool bodies in full (`true`) or in the compact
    /// head/tail-truncated form (`false`).
    tools_expanded: Cell<bool>,
    /// Render assistant thinking blocks as a single `Thinking…`
    /// placeholder (`true`) instead of the full markdown widget.
    hide_thinking_block: Cell<bool>,
    /// Render tool image attachments inline when the terminal
    /// supports an image protocol. Sourced from config at startup;
    /// has no runtime toggle today, but is session-wide so it lives
    /// here alongside the others.
    show_image_in_terminal: Cell<bool>,
    /// Bumped on every value change. Components compare it against
    /// the generation they last reconciled to decide whether to
    /// rebuild their derived caches.
    generation: Cell<u64>,
}

impl RenderSettings {
    /// Seed the settings from the host's startup configuration. The
    /// generation starts at zero; freshly-constructed components
    /// snapshot it so their first reconcile after a later toggle
    /// observes a strictly greater value.
    pub fn new(
        hide_thinking_block: bool,
        tools_expanded: bool,
        show_image_in_terminal: bool,
    ) -> Self {
        Self(Rc::new(Inner {
            tools_expanded: Cell::new(tools_expanded),
            hide_thinking_block: Cell::new(hide_thinking_block),
            show_image_in_terminal: Cell::new(show_image_in_terminal),
            generation: Cell::new(0),
        }))
    }

    /// Current generation. A component that observes a value
    /// different from the one it last reconciled knows some setting
    /// changed in between.
    pub fn generation(&self) -> u64 {
        self.0.generation.get()
    }

    pub fn tools_expanded(&self) -> bool {
        self.0.tools_expanded.get()
    }

    pub fn hide_thinking_block(&self) -> bool {
        self.0.hide_thinking_block.get()
    }

    pub fn show_image_in_terminal(&self) -> bool {
        self.0.show_image_in_terminal.get()
    }

    pub fn set_tools_expanded(&self, expanded: bool) {
        self.set(&self.0.tools_expanded, expanded);
    }

    pub fn set_hide_thinking_block(&self, hide: bool) {
        self.set(&self.0.hide_thinking_block, hide);
    }

    pub fn set_show_image_in_terminal(&self, show: bool) {
        self.set(&self.0.show_image_in_terminal, show);
    }

    /// Write `value` into `cell`, bumping the generation only on an
    /// actual change so a redundant toggle doesn't make every
    /// component re-reconcile.
    fn set(&self, cell: &Cell<bool>, value: bool) {
        if cell.get() == value {
            return;
        }
        cell.set(value);
        self.0.generation.set(self.0.generation.get() + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generation_bumps_only_on_actual_change() {
        let s = RenderSettings::new(false, false, true);
        assert_eq!(s.generation(), 0);

        // No-op writes don't move the generation.
        s.set_tools_expanded(false);
        assert_eq!(s.generation(), 0);

        // A real change bumps it once.
        s.set_tools_expanded(true);
        assert_eq!(s.generation(), 1);
        assert!(s.tools_expanded());

        // A redundant repeat is a no-op again.
        s.set_tools_expanded(true);
        assert_eq!(s.generation(), 1);

        // A different field also bumps the shared counter.
        s.set_hide_thinking_block(true);
        assert_eq!(s.generation(), 2);
        assert!(s.hide_thinking_block());
    }

    #[test]
    fn clones_share_state() {
        let a = RenderSettings::new(false, false, true);
        let b = a.clone();
        a.set_tools_expanded(true);
        // The clone observes the change and the bumped generation.
        assert!(b.tools_expanded());
        assert_eq!(b.generation(), 1);
    }
}
