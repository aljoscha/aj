//! Assistant-message component.
//!
//! Renders an in-flight or finalized assistant turn. While the
//! agent is streaming (`MessageUpdate` events flow on the bus), the
//! component owns mutable [`aj_tui::components::markdown::Markdown`]
//! widgets — one per assistant content block (thinking or text) in
//! the order the model produced them — and re-renders them as
//! bytes arrive. Once the assistant message is finalized
//! (`MessageEnd` for the live path, or a replay event for resumed
//! history), the component switches to read-only: the same widgets
//! keep painting, but the event pump no longer pushes updates into
//! them.
//!
//! A single assistant turn can carry several alternating thinking
//! and text blocks (the wire format allows it; the renderer must
//! too — the `interleaved` scripted demo exercises exactly that
//! case). Each `Start` on the bus opens a new block; subsequent
//! `Update`s append to the most recent block; `Stop` closes it.
//! Buffers and widgets are stored 1:1 with the streamed block
//! sequence so a second thinking block never clobbers the first.
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
use crate::modes::interactive::render_settings::RenderSettings;

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

/// Placeholder line shown in place of the full thinking widget.
/// A single italic line in the `thinkingText` palette colour stands
/// in for the streamed reasoning prose. Used in two cases:
///
/// * Collapsed mode (`hide_thinking_block = true`) — users who
///   don't want the verbose reasoning above every answer can still
///   see at a glance that the model thought before responding.
/// * Body-less thinking blocks — extended-thinking models routinely
///   emit a signed-but-empty thinking block ahead of tool-use
///   turns, and some provider thinking-output configurations
///   withhold the transcript entirely. In both cases the agent
///   *did* think; we surface the placeholder so the turn doesn't
///   silently look like it skipped the reasoning channel.
const HIDDEN_THINKING_LABEL: &str = "Thinking…";

/// Inline prefix prepended to the expanded thinking widget's body
/// so the rendered block is unambiguously identified as the
/// model's reasoning channel rather than part of the visible
/// answer. Kept inline (rather than emitted as a separate header
/// row) so the dim italic styling carries over the prefix and the
/// chunk reads naturally as one continuous thought, e.g.
/// `Thinking: First, let me consider …`.
const EXPANDED_THINKING_PREFIX: &str = "Thinking: ";

/// Which assistant content channel a block belongs to.
///
/// Mirrors `aj_agent::events::StreamChannel`'s text/thinking
/// distinction but is decoupled from the bus type so the renderer
/// doesn't drag the agent crate into the component's public API.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    Thinking,
    Text,
}

/// A single content block within an assistant turn.
///
/// Each `Start` event opens a new `Block`; `Update`s append to
/// [`Self::body`]; `Stop` closes it. The matching markdown
/// [`Markdown`] widget is rebuilt lazily — kept `None` until the
/// block has any body bytes and dropped again when the buffer
/// becomes empty (e.g. an aborted turn that emitted a Start
/// without any deltas).
struct Block {
    kind: BlockKind,
    body: String,
    widget: Option<Markdown>,
}

impl Block {
    fn new(kind: BlockKind, body: String) -> Self {
        Self {
            kind,
            body,
            widget: None,
        }
    }
}

/// On-screen representation of an assistant chat entry.
///
/// The component owns an ordered list of [`Block`]s — one per
/// thinking or text section the model streamed. Buffers and
/// widgets are stored 1:1 with the block sequence so a second
/// thinking block never overwrites the first, and the render
/// pass walks them in order to faithfully reproduce the
/// interleaving the agent emitted.
pub struct AssistantMessageComponent {
    /// Theme reused for every markdown widget we create so
    /// re-creating them on the fly stays cheap.
    markdown_theme: MarkdownTheme,
    /// Foreground colour applied to the expanded-mode thinking
    /// markdown and to the collapsed-mode placeholder line.
    /// Resolved through the shared theme handle so a runtime
    /// theme reload reskins existing widgets without rebuilding
    /// them.
    thinking_text: Arc<dyn Fn(&str) -> String>,
    /// Shared session-wide render settings. Read at render time
    /// (see [`Self::reconcile_settings`]) so a `aj.thinking.toggle`
    /// flip reaches this component without the toggle site walking
    /// the transcript.
    settings: RenderSettings,
    /// Generation of [`Self::settings`] that the current thinking
    /// widgets reflect. A mismatch at render time triggers a
    /// reconcile.
    last_generation: u64,
    /// Last-applied thinking-block render mode — the mode the
    /// current widgets were built for. Kept in step with
    /// `settings.hide_thinking_block()` by
    /// [`Self::reconcile_settings`]; the streaming mutators read it
    /// when (re)building widgets, and the render loop keeps it
    /// coherent because every settings change invalidates and
    /// repaints. `true` renders the thinking channel as a single
    /// `Thinking…` placeholder line instead of the full italic
    /// markdown widget; see `docs/aj-next-plan.md` §4.4.
    hide_thinking_block: bool,
    /// In-order blocks the model has streamed so far.
    blocks: Vec<Block>,
}

impl AssistantMessageComponent {
    /// Build an empty assistant-message component. The caller adds
    /// it to the chat container immediately; the markdown widgets
    /// are created on the fly when the first chunk of each block
    /// arrives. `settings` is the shared session-wide render config;
    /// the component snapshots the current thinking-fold mode for
    /// its widgets and re-reads `settings` at render time when the
    /// generation moves (the user toggling `aj.thinking.toggle`).
    pub fn new(chat_theme: &ChatTheme, settings: RenderSettings) -> Self {
        let hide_thinking_block = settings.hide_thinking_block();
        let last_generation = settings.generation();
        Self {
            markdown_theme: chat_theme.markdown.clone(),
            thinking_text: Arc::clone(&chat_theme.thinking_text),
            settings,
            last_generation,
            hide_thinking_block,
            blocks: Vec::new(),
        }
    }

    /// Open a new block of `kind`, seeded with `snapshot` (commonly
    /// empty on a real provider Start, but a non-empty payload is
    /// honoured if a provider chooses to send one). Called from
    /// `StreamChunk { action: Start { snapshot } }`.
    pub fn open_block(&mut self, kind: BlockKind, snapshot: String) {
        self.blocks.push(Block::new(kind, snapshot));
        self.refresh_last_widget();
    }

    /// Append `delta` to the most recent block of `kind`. If the
    /// most recent block is of a different kind (a protocol
    /// glitch — `Update` arriving without a matching `Start`), we
    /// implicitly open a fresh block so the delta still lands
    /// somewhere visible rather than silently dropping it.
    pub fn append_delta(&mut self, kind: BlockKind, delta: &str) {
        let needs_open = match self.blocks.last() {
            Some(b) => b.kind != kind,
            None => true,
        };
        if needs_open {
            self.blocks.push(Block::new(kind, String::new()));
        }
        let last = self.blocks.last_mut().expect("just-pushed or pre-existing");
        last.body.push_str(delta);
        Self::refresh_widget(
            last,
            &self.markdown_theme,
            &self.thinking_text,
            self.hide_thinking_block,
        );
    }

    /// Close the current block of `kind`, optionally replacing the
    /// body with `snapshot` if the provider supplied one. Called
    /// from `StreamChunk { action: Stop { snapshot } }`.
    ///
    /// The agent's `ThinkingStop` carries an empty snapshot today
    /// (it's documented as a flush signal); pass `None` in that
    /// case so we keep whatever deltas built up. A non-empty
    /// snapshot on a real provider Stop is treated as authoritative
    /// and replaces the body.
    pub fn close_block(&mut self, kind: BlockKind, snapshot: Option<String>) {
        let Some(snapshot) = snapshot else { return };
        // Walk backwards for the most recent block matching `kind`.
        // In practice it's always the tail; the fallback covers the
        // pathological "Stop without a matching Start" case.
        let Some(idx) = self.blocks.iter().rposition(|b| b.kind == kind) else {
            // No prior Start: synthesize a closed block so the
            // snapshot doesn't get lost. Matches the implicit-open
            // behaviour in `append_delta`.
            self.open_block(kind, snapshot);
            return;
        };
        let block = &mut self.blocks[idx];
        block.body = snapshot;
        Self::refresh_widget(
            block,
            &self.markdown_theme,
            &self.thinking_text,
            self.hide_thinking_block,
        );
    }

    /// Pull the latest shared render settings in and rebuild the
    /// thinking widgets when the fold/expand mode changed.
    ///
    /// Called at the top of [`Component::render`] when the settings
    /// generation has moved (the user pressed `aj.thinking.toggle`).
    /// A no-op for the widgets when the mode itself hasn't changed,
    /// so an unrelated toggle (e.g. tool expansion, which shares the
    /// generation counter) doesn't tear down a streaming markdown
    /// widget just to rebuild an identical one. In collapsed mode
    /// the markdown widget is dropped (the buffer stays the source
    /// of truth for the placeholder); flipping back rebuilds it from
    /// the preserved buffer.
    fn reconcile_settings(&mut self) {
        self.last_generation = self.settings.generation();
        let hide = self.settings.hide_thinking_block();
        if self.hide_thinking_block == hide {
            return;
        }
        self.hide_thinking_block = hide;
        for block in self.blocks.iter_mut() {
            if block.kind == BlockKind::Thinking {
                Self::refresh_widget(
                    block,
                    &self.markdown_theme,
                    &self.thinking_text,
                    self.hide_thinking_block,
                );
            }
        }
    }

    /// Whether the component has any visible content yet.
    ///
    /// A *thinking* block always counts as content, even with an
    /// empty body, because the renderer surfaces a `Thinking…`
    /// placeholder for it (extended-thinking models routinely emit
    /// signed-but-empty thinking blocks ahead of tool-use turns,
    /// and we want to acknowledge that the model thought rather
    /// than silently swallow the channel). A *text* block only
    /// counts once it has bytes, so an aborted Text Start doesn't
    /// keep an otherwise-empty assistant slot alive.
    ///
    /// The event pump consults this on `MessageEnd` to decide
    /// whether to synthesize blocks from the finalized payload
    /// (replay path): when live streaming has already produced any
    /// content, the synthesis is skipped to avoid duplicating it.
    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(|b| match b.kind {
            BlockKind::Thinking => false,
            BlockKind::Text => b.body.is_empty(),
        })
    }

    /// (Re)build the markdown widget for the most recently
    /// streamed block. Called from [`Self::open_block`] so the
    /// widget exists even when the Start carries a non-empty
    /// snapshot.
    fn refresh_last_widget(&mut self) {
        if let Some(last) = self.blocks.last_mut() {
            Self::refresh_widget(
                last,
                &self.markdown_theme,
                &self.thinking_text,
                self.hide_thinking_block,
            );
        }
    }

    /// Rebuild a single block's widget from its current body.
    ///
    /// Kept as an associated function rather than `&mut self`
    /// method so callers can iterate `self.blocks` mutably while
    /// passing the immutable theme / palette references in — the
    /// borrow checker doesn't allow holding both at once otherwise.
    fn refresh_widget(
        block: &mut Block,
        markdown_theme: &MarkdownTheme,
        thinking_text: &Arc<dyn Fn(&str) -> String>,
        hide_thinking_block: bool,
    ) {
        // Thinking blocks render through the placeholder path when
        // collapsed *or* when the body is empty (signed-but-empty
        // thinking from the provider); no markdown widget needed.
        let placeholder = matches!(block.kind, BlockKind::Thinking)
            && (hide_thinking_block || block.body.is_empty());
        if block.body.is_empty() || placeholder {
            block.widget = None;
            return;
        }
        let widget_text = match block.kind {
            // Prepend the `Thinking: ` prefix at the widget-feeding
            // stage rather than mutating `body` so the buffer stays
            // a clean transcript of what the model sent — `is_empty()`
            // and the collapsed-mode placeholder logic both want the
            // unadorned form. Strip leading whitespace from the body
            // before concatenating: Anthropic's summarized thinking
            // stream tends to start with a leading space, which
            // would otherwise render as `Thinking:  foo` (double
            // space) against our prefix's single trailing space.
            BlockKind::Thinking => {
                format!("{EXPANDED_THINKING_PREFIX}{}", block.body.trim_start())
            }
            BlockKind::Text => block.body.clone(),
        };
        let style = match block.kind {
            BlockKind::Thinking => Some(DefaultTextStyle {
                color: Some(Arc::clone(thinking_text)),
                italic: true,
                ..DefaultTextStyle::default()
            }),
            BlockKind::Text => None,
        };
        match block.widget.as_mut() {
            Some(md) => md.set_text(&widget_text),
            None => {
                block.widget = Some(Markdown::new(
                    &widget_text,
                    PADDING_X,
                    PADDING_Y,
                    markdown_theme.clone(),
                    style,
                ));
            }
        }
    }

    /// Render the collapsed-mode placeholder for a thinking block.
    /// A single italic line in the `thinkingText` colour,
    /// horizontally padded to match the markdown widget's
    /// `PADDING_X = 1` so the gutter is visually consistent
    /// regardless of mode.
    ///
    /// When `show_expand_hint` is set and the `aj.thinking.toggle`
    /// action has a configured key, the label is suffixed with
    /// `" (<key> to expand)"` so users can discover the toggle
    /// without remembering the keybinding. The hint is omitted
    /// when there's no body to expand (signed-but-empty thinking
    /// blocks, or providers configured to omit the transcript) —
    /// otherwise we'd advertise a toggle that reveals nothing.
    fn render_collapsed_thinking(&self, width: usize, show_expand_hint: bool) -> Vec<String> {
        if width <= PADDING_X {
            return Vec::new();
        }
        let gutter = " ".repeat(PADDING_X);
        let label = if show_expand_hint {
            let kb = aj_tui::keybindings::get();
            let keys = kb.get_keys(crate::config::keybindings::ACTION_THINKING_TOGGLE);
            match keys.first() {
                Some(key) => {
                    let key = aj_tui::keybindings::format_keybinding(key);
                    format!("{HIDDEN_THINKING_LABEL} ({key} to expand)")
                }
                None => HIDDEN_THINKING_LABEL.to_string(),
            }
        } else {
            HIDDEN_THINKING_LABEL.to_string()
        };
        let styled = (self.thinking_text)(&aj_tui::style::italic(&label));
        vec![format!("{gutter}{styled}")]
    }
}

impl Component for AssistantMessageComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Pull in any session-wide settings change (the user toggled
        // `aj.thinking.toggle`) before painting. Gated on the
        // generation so steady-state renders skip the work.
        if self.settings.generation() != self.last_generation {
            self.reconcile_settings();
        }
        let mut lines = Vec::new();
        // Pre-compute both placeholder variants once so we don't
        // need to re-borrow `self` while iterating `self.blocks`
        // mutably below. The `with_hint` variant is used when the
        // block has a body that an `alt+t` press would reveal;
        // `without_hint` covers body-less thinking (extended
        // thinking with the transcript omitted) where there's
        // nothing to expand.
        let collapsed_with_hint = self.render_collapsed_thinking(width, true);
        let collapsed_without_hint = self.render_collapsed_thinking(width, false);
        for block in self.blocks.iter_mut() {
            // A thinking block always renders *something* (the
            // placeholder, if collapsed or body-less); text blocks
            // are skipped when empty so an aborted/empty Text Start
            // doesn't leave a stray blank row.
            let render_placeholder = matches!(block.kind, BlockKind::Thinking)
                && (self.hide_thinking_block || block.body.is_empty());
            if block.body.is_empty() && !render_placeholder {
                continue;
            }
            // Separate consecutive blocks with a single blank row.
            // The chat container's auto-spacer handles separation
            // *between* chat children (across turns); this is the
            // intra-message break between thinking and text
            // sections.
            if !lines.is_empty() {
                lines.push(String::new());
            }
            if render_placeholder {
                // Body-less thinking blocks (signed-but-empty, or
                // providers that omit the transcript entirely) have
                // nothing to reveal — skip the "(<key> to expand)"
                // hint in that case so we don't promise a toggle
                // that produces no new content.
                let placeholder = if block.body.is_empty() {
                    &collapsed_without_hint
                } else {
                    &collapsed_with_hint
                };
                lines.extend(placeholder.iter().cloned());
            } else if let Some(w) = block.widget.as_mut() {
                lines.extend(w.render(width));
            }
        }
        lines
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        for block in self.blocks.iter_mut() {
            if let Some(w) = block.widget.as_mut() {
                w.invalidate();
            }
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
        chat_theme(&ThemeHandle::new(Theme::bundled_dark()), true)
    }

    /// Build a shared settings handle with the given thinking-fold
    /// mode (the only setting this component reads). Tests that flip
    /// the mode at runtime keep the returned handle and toggle it.
    fn settings(hide_thinking_block: bool) -> RenderSettings {
        RenderSettings::new(hide_thinking_block, false, true)
    }

    #[test]
    fn empty_thinking_block_renders_placeholder_in_expanded_mode() {
        // Extended-thinking models routinely emit a signed-but-empty
        // thinking block ahead of tool-use turns. Even in expanded
        // mode (no `hide_thinking_block`) we surface the placeholder
        // so the user can see the model did engage the reasoning
        // channel; otherwise the empty block silently vanishes.
        let mut c = AssistantMessageComponent::new(&theme(), settings(false));
        c.open_block(BlockKind::Thinking, String::new());
        c.close_block(BlockKind::Thinking, None);
        // `is_empty()` treats any thinking block as content so the
        // replay-synthesis guard in `handle_message_end` won't try
        // to re-populate from the finalized payload.
        assert!(!c.is_empty());
        let lines = c.render(80);
        assert_eq!(lines.len(), 1, "got {lines:?}");
        assert!(lines[0].contains(HIDDEN_THINKING_LABEL));
        // Body-less thinking has nothing to reveal, so the "expand"
        // hint must not be advertised — otherwise the user presses
        // `alt+t` and gets the same empty placeholder back, just
        // without the hint.
        assert!(
            !lines[0].contains("to expand"),
            "body-less placeholder should not advertise expand hint; got {:?}",
            lines[0]
        );
    }

    #[test]
    fn collapsed_thinking_with_body_advertises_expand_hint() {
        // When the user folds a non-empty thinking block, the
        // placeholder should mention the configured key so they can
        // discover how to unfold it again.
        crate::config::keybindings::install_global_manager_defaults();
        let mut c = AssistantMessageComponent::new(&theme(), settings(true));
        c.open_block(BlockKind::Thinking, "first thought".to_string());
        c.close_block(BlockKind::Thinking, None);
        let lines = c.render(80);
        assert_eq!(lines.len(), 1, "got {lines:?}");
        assert!(
            lines[0].contains("Alt+T") && lines[0].contains("to expand"),
            "expected expand hint mentioning the key; got {:?}",
            lines[0]
        );
    }

    #[test]
    fn empty_thinking_then_text_renders_placeholder_blank_and_text() {
        // The common Anthropic shape on extended-thinking tool-use
        // turns: an empty thinking block followed by the user-facing
        // text. The placeholder, the intra-message blank separator,
        // and the text body should all be present.
        let mut c = AssistantMessageComponent::new(&theme(), settings(false));
        c.open_block(BlockKind::Thinking, String::new());
        c.close_block(BlockKind::Thinking, None);
        c.open_block(BlockKind::Text, "Hello!".to_string());
        let lines = c.render(80);
        assert!(lines.len() >= 3, "got {lines:?}");
        assert!(lines[0].contains(HIDDEN_THINKING_LABEL));
        assert!(lines[1].trim().is_empty(), "expected blank separator");
        assert!(lines[2..].iter().any(|l| l.contains("Hello!")));
    }

    #[test]
    fn fresh_component_is_empty() {
        let c = AssistantMessageComponent::new(&theme(), settings(false));
        assert!(c.is_empty());
    }

    #[test]
    fn streaming_text_creates_a_widget_lazily_and_appends() {
        let mut c = AssistantMessageComponent::new(&theme(), settings(false));
        c.open_block(BlockKind::Text, "hello".to_string());
        assert!(!c.is_empty());
        assert_eq!(c.blocks.len(), 1);
        assert!(c.blocks[0].widget.is_some());

        c.append_delta(BlockKind::Text, " world");
        assert_eq!(c.blocks[0].body, "hello world");
    }

    #[test]
    fn empty_open_does_not_create_a_widget() {
        // A Start with an empty snapshot (the common case) defers
        // widget creation until the first delta arrives, so the
        // chat container doesn't render an empty bubble for a
        // turn that got cancelled between Start and any Update.
        let mut c = AssistantMessageComponent::new(&theme(), settings(false));
        c.open_block(BlockKind::Text, String::new());
        assert!(c.is_empty());
        assert!(c.blocks[0].widget.is_none());
    }

    #[test]
    fn collapsed_mode_drops_widgets_but_keeps_buffers() {
        // The collapsed-mode placeholder is emitted at render
        // time from the buffer alone; the heavy markdown widget
        // is dropped so a long replayed reasoning trace doesn't
        // sit in memory just to be hidden. The drop/rebuild now
        // happens lazily at render time when the shared settings'
        // generation moves, so each toggle is followed by a render.
        let s = settings(false);
        let mut c = AssistantMessageComponent::new(&theme(), s.clone());
        c.open_block(
            BlockKind::Thinking,
            "first I'll consider the inputs…".to_string(),
        );
        assert!(c.blocks[0].widget.is_some());
        s.set_hide_thinking_block(true);
        c.render(80);
        assert!(c.blocks[0].widget.is_none());
        assert_eq!(c.blocks[0].body, "first I'll consider the inputs…");
        // Flipping back rebuilds the markdown widget from the
        // preserved buffer so the user sees the same reasoning
        // they just hid.
        s.set_hide_thinking_block(false);
        c.render(80);
        assert!(c.blocks[0].widget.is_some());
    }

    #[test]
    fn collapsed_mode_renders_a_single_placeholder_line() {
        let mut c = AssistantMessageComponent::new(&theme(), settings(true));
        c.open_block(
            BlockKind::Thinking,
            "verbose internal reasoning".to_string(),
        );
        let lines = c.render(80);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains(HIDDEN_THINKING_LABEL));
    }

    #[test]
    fn collapsed_mode_emits_blank_row_between_thinking_and_text() {
        // The component's spacing rule (§4.4) is that exactly one
        // blank row separates a thinking block (expanded *or*
        // collapsed) from a following text block. Collapsed mode
        // must honour the same rule.
        let mut c = AssistantMessageComponent::new(&theme(), settings(true));
        c.open_block(BlockKind::Thinking, "…".to_string());
        c.open_block(BlockKind::Text, "answer".to_string());
        let lines = c.render(80);
        assert!(lines.len() >= 3, "got {lines:?}");
        assert!(lines[0].contains(HIDDEN_THINKING_LABEL));
        assert!(lines[1].trim().is_empty(), "expected blank separator");
        assert!(lines[2..].iter().any(|l| l.contains("answer")));
    }

    #[test]
    fn toggling_to_a_message_with_no_thinking_is_a_noop() {
        let s = settings(false);
        let mut c = AssistantMessageComponent::new(&theme(), s.clone());
        c.open_block(BlockKind::Text, "plain answer".to_string());
        s.set_hide_thinking_block(true);
        let lines = c.render(80);
        assert!(
            lines.iter().all(|l| !l.contains(HIDDEN_THINKING_LABEL)),
            "got {lines:?}"
        );
    }

    #[test]
    fn expanded_thinking_rendering_includes_inline_prefix() {
        let mut c = AssistantMessageComponent::new(&theme(), settings(false));
        c.open_block(
            BlockKind::Thinking,
            "the model's reasoning here".to_string(),
        );
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
    fn expanded_thinking_strips_leading_whitespace_against_prefix() {
        // Anthropic's summarized thinking stream commonly starts
        // with a leading space; the prefix already ends in one, so
        // a naive concatenation would render `Thinking:  foo` with
        // a double space. The renderer trims leading whitespace
        // from the body before concatenating; the buffer itself
        // stays untouched so the on-disk transcript is faithful.
        let mut c = AssistantMessageComponent::new(&theme(), settings(false));
        c.open_block(BlockKind::Thinking, " leading space".to_string());
        let lines = c.render(80);
        let joined = lines.join("\n");
        assert!(
            joined.contains("Thinking: leading space"),
            "expected single space between prefix and body; got {lines:?}"
        );
        assert!(
            !joined.contains("Thinking:  "),
            "expected no double space after prefix; got {lines:?}"
        );
        // Buffer remains the unadorned (untrimmed) form so it's a
        // clean transcript of what the model sent.
        assert_eq!(c.blocks[0].body, " leading space");
    }

    #[test]
    fn thinking_buffer_excludes_prefix_so_collapse_logic_stays_clean() {
        let mut c = AssistantMessageComponent::new(&theme(), settings(false));
        c.open_block(BlockKind::Thinking, "raw thought".to_string());
        assert_eq!(c.blocks[0].body, "raw thought");
        assert!(!c.is_empty());
    }

    #[test]
    fn interleaved_blocks_each_render_distinctly() {
        // Regression for the `--scripted interleaved` demo: a
        // second thinking block should not clobber the first, and
        // neither should a second text block clobber the first.
        // All four blocks must show up in the rendered output in
        // emission order.
        let mut c = AssistantMessageComponent::new(&theme(), settings(false));
        c.open_block(BlockKind::Thinking, "thought ONE".to_string());
        c.close_block(BlockKind::Thinking, None);
        c.open_block(BlockKind::Text, "reply ONE".to_string());
        c.close_block(BlockKind::Text, Some("reply ONE".to_string()));
        c.open_block(BlockKind::Thinking, "thought TWO".to_string());
        c.close_block(BlockKind::Thinking, None);
        c.open_block(BlockKind::Text, "reply TWO".to_string());
        c.close_block(BlockKind::Text, Some("reply TWO".to_string()));

        let rendered = c.render(120).join("\n");
        for needle in ["thought ONE", "reply ONE", "thought TWO", "reply TWO"] {
            assert!(
                rendered.contains(needle),
                "expected {needle:?} in rendered output; got:\n{rendered}"
            );
        }
        // Order check: each block's body must appear strictly
        // before the next one's.
        let bodies = ["thought ONE", "reply ONE", "thought TWO", "reply TWO"];
        let positions: Vec<_> = bodies
            .iter()
            .map(|b| rendered.find(b).expect("present"))
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "blocks rendered out of order; positions: {positions:?}",
        );
    }

    #[test]
    fn thinking_stop_with_empty_snapshot_preserves_deltas() {
        // The agent emits `ThinkingStop` with an empty snapshot
        // (it's a flush signal, not a finalize); the pump passes
        // `None` into `close_block` in that case, which must not
        // touch the accumulated body.
        let mut c = AssistantMessageComponent::new(&theme(), settings(false));
        c.open_block(BlockKind::Thinking, String::new());
        c.append_delta(BlockKind::Thinking, "let me think");
        c.append_delta(BlockKind::Thinking, " carefully");
        c.close_block(BlockKind::Thinking, None);
        assert_eq!(c.blocks[0].body, "let me think carefully");
    }
}
