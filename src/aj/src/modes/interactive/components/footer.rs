//! Footer — persistent bottom status line.
//!
//! Renders a single dim row underneath the editor with the model
//! name, working directory, and accumulated token usage. Stateful
//! only via the setters below so the event pump can refresh
//! individual fields as `TurnUsage` / `Notice` events arrive.
//!
//! The full footer (git branch, queue indicators, themed
//! per-section colour) lands alongside the
//! [`super::super::footer_data`] module in a follow-up commit;
//! this stub gives that work a typed slot to plug into without
//! reshaping the layout.
//!
//! See `docs/aj-next-plan.md` §4 — `components/footer.rs`.

use std::any::Any;

use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::style;

/// Thin wrapper around a single dim text row.
pub struct Footer {
    /// Provider model name, e.g. `claude-sonnet-4`.
    model: Option<String>,
    /// Working directory, displayed as the host-relative path.
    cwd: Option<String>,
    /// Compact "in / out" token tally accumulated across the
    /// session.
    usage: Option<String>,
}

impl Footer {
    /// Build an empty footer. The component renders nothing until
    /// at least one of the setters has been called.
    pub fn new() -> Self {
        Self {
            model: None,
            cwd: None,
            usage: None,
        }
    }

    /// Replace the model-name field. `None` removes it.
    pub fn set_model(&mut self, model: Option<String>) {
        self.model = model;
    }

    /// Replace the working-directory field. `None` removes it.
    pub fn set_cwd(&mut self, cwd: Option<String>) {
        self.cwd = cwd;
    }

    /// Replace the usage field. Callers pass a pre-formatted
    /// string (e.g. `"in 1.2k / out 340 / cache 12k"`) so the
    /// footer doesn't have to know about the wire usage shape.
    pub fn set_usage(&mut self, usage: Option<String>) {
        self.usage = usage;
    }
}

impl Default for Footer {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Footer {
    aj_tui::impl_component_any!();

    fn render(&mut self, _width: usize) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(m) = &self.model {
            parts.push(m.clone());
        }
        if let Some(c) = &self.cwd {
            parts.push(c.clone());
        }
        if let Some(u) = &self.usage {
            parts.push(u.clone());
        }
        if parts.is_empty() {
            return Vec::new();
        }
        // One-column left indent matches the header and the chat
        // scrollback's `padding_x = 1`, so the persistent status
        // bars share a left edge with the messages between them.
        vec![format!(" {}", style::dim(&parts.join("  ·  ")))]
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }
}

impl AsRef<dyn Any> for Footer {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_footer_renders_nothing() {
        let mut f = Footer::new();
        assert!(f.render(80).is_empty());
    }

    #[test]
    fn populated_footer_joins_fields_with_separator() {
        let mut f = Footer::new();
        f.set_model(Some("claude-sonnet-4".into()));
        f.set_cwd(Some("/home/user/proj".into()));
        let lines = f.render(80);
        assert_eq!(lines.len(), 1);
        let visible = lines[0].replace("\x1b[2m", "").replace("\x1b[22m", "");
        assert!(visible.contains("claude-sonnet-4"));
        assert!(visible.contains("/home/user/proj"));
        assert!(visible.contains("·"));
    }
}
