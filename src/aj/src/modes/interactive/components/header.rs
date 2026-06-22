//! Header — top-of-screen status banner.
//!
//! Renders a single dim line at the top of the chat scrollback
//! showing the current session id and any one-shot resume notice
//! ("Resuming conversation …"). Stateful only via
//! [`Header::set_session_id`] / [`Header::set_notice`] so the
//! event pump can refresh it as the session evolves.
//!
//! Currently minimal: the full header (model selector, cost summary,
//! sandbox banner) can plug into this scaffolding without changing the
//! layout.

use std::any::Any;

use aj_tui::ansi::truncate_to_width;
use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::style;

/// Thin wrapper around a single dim text row.
pub struct Header {
    /// Optional session id rendered on the leading edge of the
    /// header. Absent for fresh sessions where the id hasn't been
    /// allocated yet (it is, in practice — but the `Option`
    /// shape lets us defer rendering until we know).
    session_id: Option<String>,
    /// One-line notice rendered after the session id. Used for the
    /// `Resuming conversation …` banner. Cleared after the user
    /// dismisses or starts typing.
    notice: Option<String>,
}

impl Header {
    /// Build an empty header. The component renders nothing until
    /// at least one of [`Self::set_session_id`] or
    /// [`Self::set_notice`] is called.
    pub fn new() -> Self {
        Self {
            session_id: None,
            notice: None,
        }
    }

    /// Replace the session id. `None` removes it from the rendered
    /// row.
    pub fn set_session_id(&mut self, id: Option<String>) {
        self.session_id = id;
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

    fn render(&mut self, width: usize) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(id) = &self.session_id {
            parts.push(format!("session {id}"));
        }
        if let Some(n) = &self.notice {
            parts.push(n.clone());
        }
        if parts.is_empty() {
            return Vec::new();
        }
        // One-column left indent matches the chat scrollback's
        // `padding_x = 1` so the header text aligns with the
        // notices, user messages, and tool outputs below it. The
        // trailing blank row separates the persistent header from
        // the first chat child — the chat container's auto-spacer
        // only fires *between* children, not before the first one,
        // so without this blank the `Context:` notice would butt
        // directly against the banner.
        //
        // Narrow-terminal handling: the banner is a single row, so
        // we truncate (rather than wrap) when the joined content
        // doesn't fit. `truncate_to_width` keeps ANSI escapes
        // ANSI-clean and uses `…` as the visible elision marker.
        // We reserve one column for the leading indent; on a
        // zero-width terminal the row collapses to an empty
        // string. Without this clamp the strict line-width check
        // in `Tui::render` panics on terminals narrower than the
        // banner content.
        if width == 0 {
            return vec![String::new(), String::new()];
        }
        let content = parts.join("  ·  ");
        let truncated = truncate_to_width(&content, width - 1, "…", false);
        vec![format!(" {}", style::dim(&truncated)), String::new()]
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

    /// Regression: terminals narrower than the banner content used
    /// to overflow because `render` ignored `width`. The strict
    /// line-width check in `Tui::render` panicked on the first
    /// frame. The renderer must now respect `width` for every
    /// possible terminal size, truncating the visible content as
    /// needed.
    #[test]
    fn rendered_lines_never_exceed_width_for_any_width() {
        let mut h = Header::new();
        h.set_session_id(Some("2026-05-08-13-16-48-275".into()));
        h.set_notice(Some("Chat with AJ — Ctrl+C to quit".into()));
        // Sweep across the corner cases: zero, one (just the leading
        // indent), a width where the elision marker starts to bite,
        // the exact overflow the user hit (64), and a width that
        // comfortably fits the full banner.
        for width in [0usize, 1, 2, 10, 64, 65, 80, 200] {
            let lines = h.render(width);
            for (i, line) in lines.iter().enumerate() {
                let w = aj_tui::ansi::visible_width(line);
                assert!(
                    w <= width,
                    "line {i} exceeds width {width}: visible_width = {w}, line = {line:?}",
                );
            }
        }
    }

    #[test]
    fn populated_header_renders_banner_row_with_separator_and_a_trailing_blank() {
        let mut h = Header::new();
        h.set_session_id(Some("abc-123".into()));
        h.set_notice(Some("Resuming conversation".into()));
        let lines = h.render(80);
        // Banner row + trailing blank separator before the chat.
        assert_eq!(lines.len(), 2);
        assert!(
            lines[1].is_empty(),
            "second row should be a blank: {lines:?}"
        );
        // Strip ANSI for the assertion — the `dim` wrapper adds
        // SGR codes around the visible text.
        let visible = lines[0].replace("\x1b[2m", "").replace("\x1b[22m", "");
        assert!(visible.contains("session abc-123"));
        assert!(visible.contains("Resuming conversation"));
        assert!(visible.contains("·"));
    }
}
