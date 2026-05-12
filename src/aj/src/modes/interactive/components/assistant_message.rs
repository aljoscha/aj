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
//! See `docs/aj-next-plan.md` §1.1 (event protocol), §4
//! (`components/assistant_message.rs`), and §4.4
//! (thinking-block rendering modes).

use std::any::Any;
use std::sync::Arc;

use aj_tui::component::Component;
use aj_tui::components::markdown::{DefaultTextStyle, Markdown, MarkdownTheme};
use aj_tui::keys::InputEvent;

use crate::config::theme::ChatTheme;

/// Per-message vertical padding. Set to `0` so the component
/// itself doesn't emit blank rows around its content; the
/// surrounding chat container's auto-spacer (see
/// [`crate::modes::interactive::event_pump::EventPump::push_chat_child`])
/// drops a plain blank line between every two non-first chat
/// children, which is the only vertical breathing room the
/// assistant message needs. Bubble-style components (user
/// messages, tool executions) keep their internal `padding_y = 1`
/// so the bg paint extends to a one-row frame above and below
/// their content — the assistant message has no bg, so there's
/// nothing to paint and any extra blank rows would just double
/// the gap.
const PADDING_Y: usize = 0;
/// Per-message horizontal padding. One column on each side so the
/// rendered text aligns with the inset content inside bubble
/// components.
const PADDING_X: usize = 1;

/// Placeholder line shown in collapsed thinking mode. A single
/// italic line in the `thinkingText` palette colour stands in for
/// the entire streamed reasoning prose so users who don't want
/// the verbose reasoning above every answer can still see at a
/// glance that the model thought before responding.
const HIDDEN_THINKING_LABEL: &str = "Thinking…";

/// Inline prefix prepended to the expanded thinking widget's body
/// so the rendered block is unambiguously identified as the
/// model's reasoning channel rather than part of the visible
/// answer. Kept inline (rather than emitted as a separate header
/// row) so the dim italic styling carries over the prefix and the
/// chunk reads naturally as one continuous thought, e.g.
/// `Thinking: First, let me consider …`.
const EXPANDED_THINKING_PREFIX: &str = "Thinking: ";

/// On-screen representation of an assistant chat entry.
///
/// The component owns up to two markdown widgets:
///
/// - `thinking`: rendered as italic markdown in the `thinkingText`
///   palette colour, or replaced by a single-line placeholder
///   when [`Self::hide_thinking_block`] is set. Created lazily on
///   the first thinking chunk; absent for messages that didn't
///   think.
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
    markdown_theme: MarkdownTheme,
    /// Foreground colour applied to the expanded-mode thinking
    /// markdown and to the collapsed-mode placeholder line.
    /// Resolved through the shared theme handle so a runtime
    /// theme reload reskins existing widgets without rebuilding
    /// them.
    thinking_text: Arc<dyn Fn(&str) -> String>,
    /// Current cumulative snapshot of the visible text channel.
    /// Drives [`Self::text`]; written by
    /// [`Self::update_text`].
    text_buffer: String,
    /// Current cumulative snapshot of the thinking channel. Same
    /// shape as [`Self::text_buffer`] but feeds the dim/italic
    /// thinking widget.
    thinking_buffer: String,
    /// Whether to render the thinking channel as a single
    /// `Thinking…` placeholder line (collapsed mode) instead of
    /// the full italic markdown widget. Flipped at runtime by the
    /// `Ctrl+T` keybinding via
    /// [`Self::set_hide_thinking_block`]; see
    /// `docs/aj-next-plan.md` §4.4.
    hide_thinking_block: bool,
    /// Lazily-created widget for the visible text channel.
    text: Option<Markdown>,
    /// Lazily-created widget for the expanded-mode thinking
    /// channel. `None` in collapsed mode, when no thinking has
    /// streamed yet, or after the buffer is cleared by a
    /// snapshot-empty `Stop`.
    thinking: Option<Markdown>,
}

impl AssistantMessageComponent {
    /// Build an empty assistant-message component. The caller adds
    /// it to the chat container immediately; the markdown widgets
    /// are created on the fly when the first chunk of each channel
    /// arrives. `hide_thinking_block` carries the session-wide
    /// preference at the moment of construction — the host may
    /// flip it later via [`Self::set_hide_thinking_block`] in
    /// response to the `Ctrl+T` toggle.
    pub fn new(chat_theme: &ChatTheme, hide_thinking_block: bool) -> Self {
        Self {
            markdown_theme: chat_theme.markdown.clone(),
            thinking_text: Arc::clone(&chat_theme.thinking_text),
            text_buffer: String::new(),
            thinking_buffer: String::new(),
            hide_thinking_block,
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

    /// Swap the thinking-block render mode for this message.
    ///
    /// The event pump walks every assistant message in the chat
    /// container on a `Ctrl+T` toggle and calls this; the next
    /// render then shows the new mode for both finalized and
    /// in-flight messages. A no-op when the mode hasn't actually
    /// changed so we don't tear down a streaming markdown widget
    /// just to rebuild an identical one.
    pub fn set_hide_thinking_block(&mut self, hide: bool) {
        if self.hide_thinking_block == hide {
            return;
        }
        self.hide_thinking_block = hide;
        // The collapsed-mode placeholder is emitted directly from
        // `render`; the markdown widget is only kept around for
        // the expanded mode. Drop it when flipping into the
        // collapsed mode so the buffer state is the single source
        // of truth.
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
                    self.markdown_theme.clone(),
                    None,
                ));
            }
        }
    }

    /// (Re)build the inner thinking widget for the expanded mode.
    /// In collapsed mode the markdown widget is dropped and
    /// `render` emits the placeholder line directly, so this is
    /// the only path that owns the heavy widget.
    fn refresh_thinking_widget(&mut self) {
        if self.thinking_buffer.is_empty() || self.hide_thinking_block {
            self.thinking = None;
            return;
        }
        // Prepend the `Thinking: ` prefix at the widget-feeding
        // stage rather than mutating `thinking_buffer` so the
        // buffer stays a clean transcript of what the model sent —
        // `is_empty()` and the collapsed-mode placeholder logic
        // both want the unadorned form.
        let widget_text = format!("{EXPANDED_THINKING_PREFIX}{}", self.thinking_buffer);
        match self.thinking.as_mut() {
            Some(md) => md.set_text(&widget_text),
            None => {
                self.thinking = Some(Markdown::new(
                    &widget_text,
                    PADDING_X,
                    // Thinking sits flush with the upcoming text
                    // (no trailing blank line) so the answer
                    // follows naturally; the leading blank line is
                    // still rendered so it separates from the
                    // preceding turn.
                    PADDING_Y,
                    self.markdown_theme.clone(),
                    Some(DefaultTextStyle {
                        color: Some(Arc::clone(&self.thinking_text)),
                        italic: true,
                        ..DefaultTextStyle::default()
                    }),
                ));
            }
        }
    }

    /// Render the collapsed-mode placeholder for the thinking
    /// channel. A single italic line in the `thinkingText` colour,
    /// horizontally padded to match the markdown widget's
    /// `PADDING_X = 1` so the gutter is visually consistent
    /// regardless of mode.
    fn render_collapsed_thinking(&self, width: usize) -> Vec<String> {
        // Match the markdown widget's leading-column gutter so
        // toggling modes doesn't shift the text horizontally.
        if width <= PADDING_X {
            return Vec::new();
        }
        let gutter = " ".repeat(PADDING_X);
        let styled = (self.thinking_text)(&aj_tui::style::italic(HIDDEN_THINKING_LABEL));
        vec![format!("{gutter}{styled}")]
    }
}

impl Component for AssistantMessageComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Thinking renders first so the visible answer follows
        // it (matching the natural reading order: "let me think
        // …" then the answer). With `padding_y = 0` on both
        // widgets the two would butt directly against each
        // other; a single explicit blank row between them keeps
        // the two channels visually distinct.
        let mut lines = Vec::new();
        if self.hide_thinking_block {
            if !self.thinking_buffer.is_empty() {
                lines.extend(self.render_collapsed_thinking(width));
            }
        } else if let Some(thinking) = self.thinking.as_mut() {
            lines.extend(thinking.render(width));
        }
        if let Some(text) = self.text.as_mut() {
            if !lines.is_empty() {
                lines.push(String::new());
            }
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
    use crate::config::theme::{Theme, ThemeHandle, chat_theme};

    fn theme() -> ChatTheme {
        chat_theme(&ThemeHandle::new(Theme::bundled_dark()))
    }

    #[test]
    fn fresh_component_is_empty() {
        let c = AssistantMessageComponent::new(&theme(), false);
        assert!(c.is_empty());
    }

    #[test]
    fn streaming_text_creates_a_widget_lazily_and_appends() {
        let mut c = AssistantMessageComponent::new(&theme(), false);
        c.set_text_snapshot("hello".to_string());
        assert!(!c.is_empty());
        assert!(c.text.is_some());

        c.append_text_delta(" world");
        assert_eq!(c.text_buffer, "hello world");
    }

    #[test]
    fn empty_snapshot_drops_the_inner_widget() {
        let mut c = AssistantMessageComponent::new(&theme(), false);
        c.set_text_snapshot("hello".to_string());
        assert!(c.text.is_some());
        // A `Stop` carrying an empty snapshot should clear out the
        // inner widget so the chat container doesn't render a stale
        // bubble for an aborted turn.
        c.set_text_snapshot(String::new());
        assert!(c.text.is_none());
        assert!(c.is_empty());
    }

    #[test]
    fn collapsed_mode_drops_the_thinking_widget_but_keeps_the_buffer() {
        // The collapsed-mode placeholder is emitted at render
        // time from the buffer alone; the heavy markdown widget
        // is dropped so a long replayed reasoning trace doesn't
        // sit in memory just to be hidden.
        let mut c = AssistantMessageComponent::new(&theme(), false);
        c.set_thinking_snapshot("first I'll consider the inputs…".to_string());
        assert!(c.thinking.is_some());
        c.set_hide_thinking_block(true);
        assert!(c.thinking.is_none());
        assert_eq!(c.thinking_buffer, "first I'll consider the inputs…");
        // Flipping back rebuilds the markdown widget from the
        // preserved buffer so the user sees the same reasoning
        // they just hid.
        c.set_hide_thinking_block(false);
        assert!(c.thinking.is_some());
    }

    #[test]
    fn collapsed_mode_renders_a_single_placeholder_line() {
        let mut c = AssistantMessageComponent::new(&theme(), true);
        c.set_thinking_snapshot("verbose internal reasoning".to_string());
        let lines = c.render(80);
        assert_eq!(lines.len(), 1);
        // The line carries the literal placeholder text
        // regardless of how long the snapshot is. The leading
        // gutter and any ANSI escapes are part of the rendered
        // string; we assert the visible payload is the constant.
        assert!(lines[0].contains(HIDDEN_THINKING_LABEL));
    }

    #[test]
    fn collapsed_mode_emits_blank_row_between_thinking_and_text() {
        // The component's spacing rule (§4.4) is that exactly one
        // blank row separates a thinking block (expanded *or*
        // collapsed) from a following text block. Collapsed mode
        // must honor the same rule.
        let mut c = AssistantMessageComponent::new(&theme(), true);
        c.set_thinking_snapshot("…".to_string());
        c.set_text_snapshot("answer".to_string());
        let lines = c.render(80);
        // Placeholder line, blank separator, then ≥1 text line.
        assert!(lines.len() >= 3, "got {lines:?}");
        assert!(lines[0].contains(HIDDEN_THINKING_LABEL));
        assert!(lines[1].trim().is_empty(), "expected blank separator");
        assert!(lines[2..].iter().any(|l| l.contains("answer")));
    }

    #[test]
    fn toggling_to_a_message_with_no_thinking_is_a_noop() {
        // A finalized text-only assistant message shouldn't grow
        // a placeholder when the user flips into collapsed mode;
        // the placeholder is only emitted when there's actual
        // thinking content to stand in for.
        let mut c = AssistantMessageComponent::new(&theme(), false);
        c.set_text_snapshot("plain answer".to_string());
        c.set_hide_thinking_block(true);
        let lines = c.render(80);
        assert!(
            lines.iter().all(|l| !l.contains(HIDDEN_THINKING_LABEL)),
            "got {lines:?}"
        );
    }

    #[test]
    fn expanded_thinking_rendering_includes_inline_prefix() {
        // The expanded thinking block is prefixed with
        // `Thinking: ` inline so users can tell the reasoning
        // channel apart from the visible answer at a glance.
        let mut c = AssistantMessageComponent::new(&theme(), false);
        c.set_thinking_snapshot("the model's reasoning here".to_string());
        let lines = c.render(80);
        let joined = lines.join("\n");
        assert!(
            joined.contains("Thinking: "),
            "expected 'Thinking: ' prefix in expanded thinking render; got {lines:?}"
        );
        assert!(
            joined.contains("the model's reasoning here"),
            "expected the thinking body to still be rendered alongside the prefix; got {lines:?}"
        );
    }

    #[test]
    fn thinking_buffer_excludes_prefix_so_collapse_logic_stays_clean() {
        // The prefix is purely a render-time decoration; the
        // stored buffer keeps the unadorned transcript so
        // `is_empty()` and the collapse-mode placeholder logic
        // see exactly what the model sent.
        let mut c = AssistantMessageComponent::new(&theme(), false);
        c.set_thinking_snapshot("raw thought".to_string());
        assert_eq!(c.thinking_buffer, "raw thought");
        assert!(!c.is_empty());
    }
}
