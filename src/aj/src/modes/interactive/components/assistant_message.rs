//! Assistant-message component.
//!
//! Renders an in-flight or finalized assistant turn. While the
//! agent is streaming (`StreamChunk` events flow on the bus), the
//! component owns mutable [`aj_tui::components::markdown::Markdown`]
//! widgets — one for the visible text channel, one for the
//! extended-thinking channel — and re-renders them as bytes
//! arrive. Once the assistant message is finalized
//! (`MessagePersisted::Assistant` for the live path, or a replay
//! event for resumed history), the component switches to read-only:
//! the same widgets keep painting, but the event pump no longer
//! pushes updates into them.
//!
//! See `docs/aj-next-plan.md` §1.1 (event protocol) and §4
//! (`components/assistant_message.rs`).

use std::any::Any;

use aj_tui::component::Component;
use aj_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use aj_tui::keys::InputEvent;

/// Per-message vertical padding shared with the user message
/// component (one blank line above + below).
const PADDING_Y: usize = 1;
/// Per-message horizontal padding (one column on each side).
const PADDING_X: usize = 1;

/// On-screen representation of an assistant chat entry.
///
/// The component owns up to two markdown widgets:
///
/// - `thinking`: rendered as dim italics so the user can scan past
///   reasoning blocks if they don't want them. Created lazily on the
///   first thinking chunk; absent for messages that didn't think.
/// - `text`: the model's visible answer. Created lazily on the
///   first text chunk; absent until the assistant produces visible
///   output (e.g. a tool-call-only turn surfaces no `text` widget).
///
/// Updates flow in via [`AssistantMessageComponent::update_text`]
/// and [`AssistantMessageComponent::update_thinking`]; the event
/// pump calls these from `StreamChunk`'s `Start`/`Update`/`Stop`
/// actions. Both methods take the *cumulative* snapshot for that
/// channel — the agent's stream chunk semantics already give us a
/// full snapshot on `Start`/`Stop`, and we reconstruct the running
/// snapshot on `Update` by appending the delta to our own buffer.
pub struct AssistantMessageComponent {
    /// Theme reused for both the text and thinking markdown
    /// widgets so re-creating them on the fly stays cheap.
    theme: MarkdownTheme,
    /// Current cumulative snapshot of the visible text channel.
    /// Drives [`Self::text`]; written by
    /// [`Self::update_text`].
    text_buffer: String,
    /// Current cumulative snapshot of the thinking channel. Same
    /// shape as [`Self::text_buffer`] but feeds the dim/italic
    /// thinking widget.
    thinking_buffer: String,
    /// Lazily-created widget for the visible text channel.
    text: Option<Markdown>,
    /// Lazily-created widget for the thinking channel.
    thinking: Option<Markdown>,
}

impl AssistantMessageComponent {
    /// Build an empty assistant-message component. The caller adds
    /// it to the chat container immediately; the markdown widgets
    /// are created on the fly when the first chunk of each channel
    /// arrives.
    pub fn new(theme: MarkdownTheme) -> Self {
        Self {
            theme,
            text_buffer: String::new(),
            thinking_buffer: String::new(),
            text: None,
            thinking: None,
        }
    }

    /// Replace the visible-text snapshot with `snapshot` and
    /// re-render. Called from `StreamChunk { channel: Text,
    /// action: Start | Stop }`; the cumulative snapshot is
    /// authoritative on those edges.
    pub fn set_text_snapshot(&mut self, snapshot: String) {
        self.text_buffer = snapshot;
        self.refresh_text_widget();
    }

    /// Append `delta` to the current visible-text snapshot and
    /// re-render. Called from `StreamChunk { channel: Text, action:
    /// Update { delta } }` on the streaming bridge path.
    pub fn append_text_delta(&mut self, delta: &str) {
        self.text_buffer.push_str(delta);
        self.refresh_text_widget();
    }

    /// Replace the thinking snapshot with `snapshot`. See
    /// [`Self::set_text_snapshot`] for the live-vs-edge semantics.
    pub fn set_thinking_snapshot(&mut self, snapshot: String) {
        self.thinking_buffer = snapshot;
        self.refresh_thinking_widget();
    }

    /// Append `delta` to the current thinking snapshot.
    pub fn append_thinking_delta(&mut self, delta: &str) {
        self.thinking_buffer.push_str(delta);
        self.refresh_thinking_widget();
    }

    /// Whether the component has any visible content yet. The
    /// event pump uses this to decide whether to drop a freshly-
    /// created assistant container that turned out to carry only
    /// tool calls — those have no text or thinking blocks of their
    /// own and would render as a blank gap in the chat scrollback.
    pub fn is_empty(&self) -> bool {
        self.text_buffer.is_empty() && self.thinking_buffer.is_empty()
    }

    /// (Re)build the inner text widget from
    /// [`Self::text_buffer`]. Called on every chunk; the
    /// markdown widget's `set_text` swaps the source and triggers
    /// a render-state invalidation cheaply.
    fn refresh_text_widget(&mut self) {
        if self.text_buffer.is_empty() {
            self.text = None;
            return;
        }
        match self.text.as_mut() {
            Some(md) => md.set_text(&self.text_buffer),
            None => {
                self.text = Some(Markdown::new(
                    &self.text_buffer,
                    PADDING_X,
                    PADDING_Y,
                    self.theme.clone(),
                    None,
                ));
            }
        }
    }

    /// (Re)build the inner thinking widget. Same shape as
    /// [`Self::refresh_text_widget`] but applies a dim+italic
    /// outer style so reasoning prose visually fades against the
    /// answer.
    fn refresh_thinking_widget(&mut self) {
        if self.thinking_buffer.is_empty() {
            self.thinking = None;
            return;
        }
        match self.thinking.as_mut() {
            Some(md) => md.set_text(&self.thinking_buffer),
            None => {
                use std::sync::Arc;
                self.thinking = Some(Markdown::new(
                    &self.thinking_buffer,
                    PADDING_X,
                    // Thinking sits flush with the upcoming text
                    // (no trailing blank line) so the answer
                    // follows naturally; the leading blank line is
                    // still rendered so it separates from the
                    // preceding turn.
                    PADDING_Y,
                    self.theme.clone(),
                    Some(DefaultTextStyle {
                        color: Some(Arc::new(aj_tui::style::dim)),
                        italic: true,
                        ..DefaultTextStyle::default()
                    }),
                ));
            }
        }
    }
}

impl Component for AssistantMessageComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Thinking renders first so the visible answer follows
        // it (matching the natural reading order: "let me think
        // …" then the answer).
        let mut lines = Vec::new();
        if let Some(thinking) = self.thinking.as_mut() {
            lines.extend(thinking.render(width));
        }
        if let Some(text) = self.text.as_mut() {
            lines.extend(text.render(width));
        }
        lines
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        if let Some(t) = self.thinking.as_mut() {
            t.invalidate();
        }
        if let Some(t) = self.text.as_mut() {
            t.invalidate();
        }
    }
}

impl AsRef<dyn Any> for AssistantMessageComponent {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::theme::{Theme, ThemeHandle, markdown_theme};

    fn theme() -> MarkdownTheme {
        markdown_theme(&ThemeHandle::new(Theme::bundled_dark()))
    }

    #[test]
    fn fresh_component_is_empty() {
        let c = AssistantMessageComponent::new(theme());
        assert!(c.is_empty());
    }

    #[test]
    fn streaming_text_creates_a_widget_lazily_and_appends() {
        let mut c = AssistantMessageComponent::new(theme());
        c.set_text_snapshot("hello".to_string());
        assert!(!c.is_empty());
        assert!(c.text.is_some());

        c.append_text_delta(" world");
        assert_eq!(c.text_buffer, "hello world");
    }

    #[test]
    fn empty_snapshot_drops_the_inner_widget() {
        let mut c = AssistantMessageComponent::new(theme());
        c.set_text_snapshot("hello".to_string());
        assert!(c.text.is_some());
        // A `Stop` carrying an empty snapshot should clear out the
        // inner widget so the chat container doesn't render a stale
        // bubble for an aborted turn.
        c.set_text_snapshot(String::new());
        assert!(c.text.is_none());
        assert!(c.is_empty());
    }
}
