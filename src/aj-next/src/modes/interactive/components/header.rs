//! Header — top-of-screen status banner.
//!
//! Renders a single dim line at the top of the chat scrollback
//! showing the current thread id and any one-shot resume notice
//! ("Resuming conversation …"). Stateful only via
//! [`Header::set_thread_id`] / [`Header::set_notice`] so the
//! event pump can refresh it as the session evolves.
//!
//! The full header (model selector, cost summary,
//! sandbox banner) lands alongside the "Selectors and theming"
//! Phase 1 step; this scaffolding gives that work a place to
//! plug into without changing the layout.
//!
//! See `docs/aj-next-plan.md` §4 — `components/header.rs`.

use std::any::Any;

use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::style;

/// Thin wrapper around a single dim text row.
pub struct Header {
    /// Optional thread id rendered on the leading edge of the
    /// header. Absent for fresh threads where the id hasn't been
    /// allocated yet (it is, in practice — but the `Option`
    /// shape lets us defer rendering until we know).
    thread_id: Option<String>,
    /// One-line notice rendered after the thread id. Used for the
    /// `Resuming conversation …` banner. Cleared after the user
    /// dismisses or starts typing.
    notice: Option<String>,
}

impl Header {
    /// Build an empty header. The component renders nothing until
    /// at least one of [`Self::set_thread_id`] or
    /// [`Self::set_notice`] is called.
    pub fn new() -> Self {
        Self {
            thread_id: None,
            notice: None,
        }
    }

    /// Replace the thread id. `None` removes it from the rendered
    /// row.
    pub fn set_thread_id(&mut self, id: Option<String>) {
        self.thread_id = id;
    }

    /// Replace the one-line notice. `None` removes it.
    pub fn set_notice(&mut self, notice: Option<String>) {
        self.notice = notice;
    }
}

impl Default for Header {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Header {
    aj_tui::impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(id) = &self.thread_id {
            parts.push(format!("thread {id}"));
        }
        if let Some(n) = &self.notice {
            parts.push(n.clone());
        }
        if parts.is_empty() {
            return Vec::new();
        }
        vec![style::dim(&parts.join("  ·  "))]
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }
}

impl AsRef<dyn Any> for Header {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_header_renders_nothing() {
        let mut h = Header::new();
        assert!(h.render(80).is_empty());
    }

    #[test]
    fn populated_header_renders_one_line_with_separator() {
        let mut h = Header::new();
        h.set_thread_id(Some("abc-123".into()));
        h.set_notice(Some("Resuming conversation".into()));
        let lines = h.render(80);
        assert_eq!(lines.len(), 1);
        // Strip ANSI for the assertion — the `dim` wrapper adds
        // SGR codes around the visible text.
        let visible = lines[0].replace("\x1b[2m", "").replace("\x1b[22m", "");
        assert!(visible.contains("thread abc-123"));
        assert!(visible.contains("Resuming conversation"));
        assert!(visible.contains("·"));
    }
}
