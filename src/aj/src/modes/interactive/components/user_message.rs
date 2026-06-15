//! User-message component.
//!
//! Renders a single user-role chat entry — the typed prompt that
//! triggered the current turn or a replayed user-thread message.
//!
//! The component wraps an [`aj_tui::components::markdown::Markdown`]
//! widget and tints every row through the shared
//! [`ChatTheme::user_message_bg`] closure so the bubble reads as a
//! single coloured rectangle inset by one column on each side. No
//! quote marker (`> ` / `│ `) is prepended: the background tint is
//! the entire visual cue, which also keeps the rendered text
//! cleanly copy-pasteable.
//!
//! See `docs/aj-next-plan.md` §4 — `components/user_message.rs`.

use std::any::Any;
use std::sync::Arc;

use aj_tui::component::Component;
use aj_tui::components::markdown::{DefaultTextStyle, Markdown};
use aj_tui::keys::InputEvent;

use crate::config::theme::ChatTheme;

/// Per-message vertical padding shared with the assistant message
/// component. One blank line above and below each message gives
/// the user a clean visual break between turns; both rows are
/// background-painted by the markdown widget's row-emission stage
/// so the bubble extends through them.
const PADDING_Y: usize = 1;
/// Per-message horizontal padding. One column on each side so the
/// tinted rectangle has a small inset from the terminal edges.
const PADDING_X: usize = 1;

/// On-screen representation of a user-role chat entry.
///
/// Held inside the chat scrollback container; the event pump owns
/// the `Box<dyn Component>` and never reads it back out, so we
/// don't need any mutating accessors today.
pub struct UserMessageComponent {
    markdown: Markdown,
}

impl UserMessageComponent {
    /// Build a user-message component rendering `text` through the
    /// shared chat theme. The `bg_color` in [`DefaultTextStyle`]
    /// hands the markdown widget the closure it routes every
    /// rendered row (including the top and bottom padding rows)
    /// through, so the entire bubble rectangle picks up the user
    /// message background tint.
    pub fn new(text: &str, theme: &ChatTheme) -> Self {
        let markdown = Markdown::new(
            text,
            PADDING_X,
            PADDING_Y,
            theme.markdown.clone(),
            Some(DefaultTextStyle {
                bg_color: Some(Arc::clone(&theme.user_message_bg)),
                ..DefaultTextStyle::default()
            }),
        );
        Self { markdown }
    }
}

impl Component for UserMessageComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        self.markdown.render(width)
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        self.markdown.invalidate();
    }
}

impl AsRef<dyn Any> for UserMessageComponent {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::theme::{Theme, ThemeHandle, chat_theme};

    fn theme() -> ChatTheme {
        chat_theme(&ThemeHandle::new(Theme::bundled_dark()), true)
    }

    #[test]
    fn renders_text_without_a_quote_prefix() {
        // Regression: we used to prepend `> ` to every line so the
        // markdown renderer styled the message as a blockquote.
        // That produced a `│ ` border glyph the user couldn't
        // exclude when copying the prompt back out. The new bubble
        // relies entirely on the background tint, so no `│` should
        // appear in the rendered output.
        let mut c = UserMessageComponent::new("hello world", &theme());
        let lines = c.render(40);
        for line in &lines {
            assert!(
                !line.contains('│'),
                "user message should not render a blockquote border, got: {line:?}",
            );
        }
    }

    #[test]
    fn rendered_rows_carry_the_user_message_background_escape() {
        // The bundled dark theme's `userMsgBg` resolves to a
        // truecolor (`\x1b[48;2;…m`) escape on capable terminals
        // and a 256-color (`\x1b[48;5;…m`) escape elsewhere. Both
        // share the `\x1b[48;` prefix, so checking for that is a
        // robust assertion that the background tint actually
        // reached the rendered rows.
        let mut c = UserMessageComponent::new("hi", &theme());
        let lines = c.render(20);
        assert!(
            lines.iter().any(|l| l.contains("\x1b[48;")),
            "expected at least one row carrying a background escape, got: {lines:?}",
        );
    }
}
