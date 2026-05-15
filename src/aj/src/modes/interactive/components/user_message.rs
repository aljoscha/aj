//! User-message component.
//!
//! Renders a single user-role chat entry — the typed prompt that
//! triggered the current turn or a replayed user-thread message.
//!
//! Wraps an [`aj_tui::components::markdown::Markdown`] with a
//! `> ` prefix prepended to every paragraph so user messages are
//! visually distinguishable from assistant output. The Markdown
//! widget handles wrapping, code-block highlighting, and the
//! padding rhythm shared with the assistant message component.
//!
//! See `docs/aj-next-plan.md` §4 — `components/user_message.rs`.

use std::any::Any;

use aj_tui::component::Component;
use aj_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use aj_tui::keys::InputEvent;

/// Per-message vertical padding shared with the assistant message
/// component. One blank line above and below each message gives
/// the user a clean visual break between turns.
const PADDING_Y: usize = 1;
/// Per-message horizontal padding. One column on each side so the
/// rendered text has a small inset from the terminal edges.
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
    /// shared markdown theme.
    pub fn new(text: &str, theme: MarkdownTheme) -> Self {
        // Prefix every line with `> ` so the message stands out
        // without needing a dedicated colour or border. The
        // Markdown widget wraps each paragraph and re-applies the
        // prefix on every wrapped line, so multi-line user input
        // stays visually grouped.
        let quoted = prefix_user_lines(text);
        let markdown = Markdown::new(
            &quoted,
            PADDING_X,
            PADDING_Y,
            theme,
            // Italicise the entire message so the prefixed `>` plus
            // the slanted body reads unmistakably as user input —
            // the assistant message component leaves text upright.
            Some(DefaultTextStyle {
                italic: true,
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

/// Prepend `> ` to every line of `text` so the markdown renderer
/// styles the message as a quoted block. Empty lines stay empty so
/// paragraph breaks survive the transformation.
fn prefix_user_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.lines().count() * 2);
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.is_empty() {
            continue;
        }
        out.push_str("> ");
        out.push_str(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_each_non_empty_line_with_a_quote_marker() {
        let out = prefix_user_lines("hello\nworld");
        assert_eq!(out, "> hello\n> world");
    }

    #[test]
    fn preserves_blank_lines_as_paragraph_breaks() {
        let out = prefix_user_lines("a\n\nb");
        assert_eq!(out, "> a\n\n> b");
    }

    #[test]
    fn handles_a_trailing_newline_without_orphan_prefix() {
        let out = prefix_user_lines("hi\n");
        // `split('\n')` surfaces the empty trailing element; we
        // intentionally render it as an empty line rather than as
        // a prefix-only `> ` so trailing whitespace doesn't leak.
        assert_eq!(out, "> hi\n");
    }
}
