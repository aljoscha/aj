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

use aj_tui::ansi::truncate_to_width;
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

    fn render(&mut self, width: usize) -> Vec<String> {
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
        //
        // Narrow-terminal handling: a long cwd or model name can
        // easily push the joined row past the terminal width, so
        // we truncate with an ellipsis instead of overflowing.
        // Wrapping would push the editor up a row and shift the
        // hardware cursor away from where the diff engine expects
        // it; a single truncated row keeps the layout stable.
        if width == 0 {
            return vec![String::new()];
        }
        let content = parts.join("  ·  ");
        let truncated = truncate_to_width(&content, width - 1, "…", false);
        vec![format!(" {}", style::dim(&truncated))]
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

    /// Regression: a long cwd or model name on a narrow terminal
    /// used to overflow because `render` ignored `width`. Mirrors
    /// the equivalent test in `header.rs`.
    #[test]
    fn rendered_lines_never_exceed_width_for_any_width() {
        let mut f = Footer::new();
        f.set_model(Some("claude-sonnet-4-very-long-model-name".into()));
        f.set_cwd(Some("/home/user/some/deeply/nested/project/path".into()));
        f.set_usage(Some("in 12.3k / out 4.5k / cache 6.7k".into()));
        for width in [0usize, 1, 2, 10, 40, 64, 80, 200] {
            let lines = f.render(width);
            for (i, line) in lines.iter().enumerate() {
                let w = aj_tui::ansi::visible_width(line);
                assert!(
                    w <= width,
                    "line {i} exceeds width {width}: visible_width = {w}, line = {line:?}",
                );
            }
        }
    }
}
