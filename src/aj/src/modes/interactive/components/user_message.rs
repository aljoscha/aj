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
//! Harness-injected task-completion notices (see
//! `aj_agent::Agent::drain_task_notices`) also arrive as user messages.
//! They can be long, so when one is detected the component is built in a
//! foldable mode: collapsed it shows a short preview plus an expand
//! hint, and it expands and collapses together with tool output under
//! the shared `aj.tools.expand` toggle, mirroring
//! [`CompactionSummaryComponent`](super::compaction_summary::CompactionSummaryComponent).
//! Ordinary typed or replayed prompts are never foldable.

use std::any::Any;
use std::sync::Arc;

use aj_tui::component::Component;
use aj_tui::components::markdown::{DefaultTextStyle, Markdown};
use aj_tui::keys::InputEvent;

use crate::config::theme::ChatTheme;
use crate::modes::interactive::components::tool_execution::{HintKind, expand_hint_text};
use crate::modes::interactive::render_settings::RenderSettings;

/// Per-message vertical padding shared with the assistant message
/// component. One blank line above and below each message gives
/// the user a clean visual break between turns; both rows are
/// background-painted by the markdown widget's row-emission stage
/// so the bubble extends through them.
const PADDING_Y: usize = 1;
/// Per-message horizontal padding. One column on each side so the
/// tinted rectangle has a small inset from the terminal edges.
const PADDING_X: usize = 1;

/// Source-line count shown for a foldable message while collapsed. A
/// short preview keeps a long task notification from flooding the
/// scrollback while still surfacing its first (and most informative)
/// line; revealing the rest is one `aj.tools.expand` keystroke away.
const COLLAPSED_LINES: usize = 10;

/// On-screen representation of a user-role chat entry.
///
/// Held inside the chat scrollback container; the event pump owns
/// the `Box<dyn Component>` and never reads it back out, so we
/// don't need any mutating accessors today.
pub struct UserMessageComponent {
    markdown: Markdown,
    /// Fold state for harness-injected messages (task notifications).
    /// `None` for ordinary typed or replayed prompts, which always
    /// render in full.
    fold: Option<Fold>,
}

/// Collapse state for a foldable user message.
struct Fold {
    /// Full message text, kept as the source of truth so a toggle can
    /// rebuild the markdown widget with either the preview or the full
    /// body.
    text: String,
    theme: ChatTheme,
    /// Shared session-wide render settings; read at render time so an
    /// `aj.tools.expand` flip reaches this component without the toggle
    /// site walking the transcript.
    settings: RenderSettings,
    /// Expansion mode the cached `markdown` reflects.
    expanded: bool,
    /// Generation of [`Self::settings`] the cached `markdown` reflects.
    last_generation: u64,
}

impl UserMessageComponent {
    /// Build a user-message component rendering `text` through the
    /// shared chat theme. The `bg_color` in [`DefaultTextStyle`]
    /// hands the markdown widget the closure it routes every
    /// rendered row (including the top and bottom padding rows)
    /// through, so the entire bubble rectangle picks up the user
    /// message background tint.
    pub fn new(text: &str, theme: &ChatTheme) -> Self {
        Self {
            markdown: build_markdown(text, theme),
            fold: None,
        }
    }

    /// Build a foldable user-message component for a harness-injected
    /// message. Collapsed (the default, since the shared
    /// `tools_expanded` setting starts compact) it renders only a short
    /// preview plus an expand hint; expanding follows `aj.tools.expand`.
    pub fn new_collapsible(text: &str, theme: &ChatTheme, settings: RenderSettings) -> Self {
        let expanded = settings.tools_expanded();
        let last_generation = settings.generation();
        Self {
            markdown: build_markdown(&fold_source(text, expanded), theme),
            fold: Some(Fold {
                text: text.to_string(),
                theme: theme.clone(),
                settings,
                expanded,
                last_generation,
            }),
        }
    }
}

/// Build the bubble's markdown widget for `text`. The `bg_color` closure
/// is what paints every emitted row (content and padding) so the whole
/// rectangle picks up the user-message background tint.
fn build_markdown(text: &str, theme: &ChatTheme) -> Markdown {
    Markdown::new(
        text,
        PADDING_X,
        PADDING_Y,
        theme.markdown.clone(),
        Some(DefaultTextStyle {
            bg_color: Some(Arc::clone(&theme.user_message_bg)),
            ..DefaultTextStyle::default()
        }),
    )
}

/// Markdown source for a foldable message: the full `text` when
/// `expanded` or already short, otherwise the first [`COLLAPSED_LINES`]
/// source lines followed by an expand hint.
///
/// The hint is emitted as plain (unstyled) text wrapped in markdown
/// emphasis rather than a `style::dim` escape. We feed this string back
/// through the markdown widget, which would render an embedded ANSI
/// escape literally, and italic keeps the hint visually muted while
/// still inheriting the bubble's background tint.
fn fold_source(text: &str, expanded: bool) -> String {
    if expanded {
        return text.to_string();
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= COLLAPSED_LINES {
        return text.to_string();
    }
    let more = lines.len() - COLLAPSED_LINES;
    let shown = lines[..COLLAPSED_LINES].join("\n");
    let hint = expand_hint_text(more, HintKind::More);
    format!("{shown}\n\n*{hint}*")
}

impl Component for UserMessageComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        // Pull in an `aj.tools.expand` toggle before painting, gated on
        // the generation so steady-state renders skip the rebuild. Only
        // an actual mode change rebuilds the widget; an unrelated toggle
        // (e.g. thinking fold) shares the generation counter but leaves
        // `expanded` unchanged.
        if let Some(fold) = self.fold.as_mut() {
            if fold.settings.generation() != fold.last_generation {
                fold.last_generation = fold.settings.generation();
                let expanded = fold.settings.tools_expanded();
                if expanded != fold.expanded {
                    fold.expanded = expanded;
                    let source = fold_source(&fold.text, expanded);
                    self.markdown = build_markdown(&source, &fold.theme);
                }
            }
        }
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

    /// `RenderSettings` with the given tool-expansion mode (the only
    /// setting the foldable path reads).
    fn settings(tools_expanded: bool) -> RenderSettings {
        RenderSettings::new(false, tools_expanded, true)
    }

    fn strip_ansi(s: &str) -> String {
        let mut out: Vec<u8> = Vec::with_capacity(s.len());
        let bytes = s.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'm' {
                    i += 1;
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).expect("surviving bytes remain valid UTF-8")
    }

    /// A task notification with a recognisable first line and a marker
    /// well past [`COLLAPSED_LINES`] so tests can distinguish the
    /// preview from the full body.
    fn notification() -> String {
        let mut lines = vec!["Background task #1 finished: sleep — exit code 0".to_string()];
        for i in 1..30 {
            lines.push(format!("tick {i}"));
        }
        lines.push("SECRET_TAIL_MARKER".to_string());
        format!(
            "<task-notification>\n{}\n</task-notification>",
            lines.join("\n")
        )
    }

    fn rendered(c: &mut UserMessageComponent, width: usize) -> String {
        c.render(width).iter().map(|l| strip_ansi(l)).collect()
    }

    #[test]
    fn collapsed_notification_shows_preview_and_expand_hint() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut c =
            UserMessageComponent::new_collapsible(&notification(), &theme(), settings(false));
        let joined = rendered(&mut c, 120);
        assert!(joined.contains("Background task #1 finished"), "{joined:?}");
        assert!(!joined.contains("SECRET_TAIL_MARKER"), "{joined:?}");
        assert!(joined.contains("to expand"), "{joined:?}");
    }

    #[test]
    fn expanded_notification_shows_full_body() {
        let mut c =
            UserMessageComponent::new_collapsible(&notification(), &theme(), settings(true));
        let joined = rendered(&mut c, 120);
        assert!(joined.contains("SECRET_TAIL_MARKER"), "{joined:?}");
        assert!(!joined.contains("to expand"), "{joined:?}");
    }

    #[test]
    fn toggling_expansion_reveals_then_hides_the_tail() {
        let s = settings(false);
        let mut c = UserMessageComponent::new_collapsible(&notification(), &theme(), s.clone());
        assert!(!rendered(&mut c, 120).contains("SECRET_TAIL_MARKER"));

        s.set_tools_expanded(true);
        assert!(rendered(&mut c, 120).contains("SECRET_TAIL_MARKER"));

        s.set_tools_expanded(false);
        assert!(!rendered(&mut c, 120).contains("SECRET_TAIL_MARKER"));
    }

    #[test]
    fn short_notification_is_not_truncated() {
        let text = "<task-notification>\nBackground task #1 finished: sleep — exit code 0\n</task-notification>";
        let mut c = UserMessageComponent::new_collapsible(text, &theme(), settings(false));
        let joined = rendered(&mut c, 120);
        assert!(joined.contains("Background task #1 finished"), "{joined:?}");
        // Nothing was hidden, so no expand hint should be advertised.
        assert!(!joined.contains("to expand"), "{joined:?}");
    }

    #[test]
    fn renders_text_without_a_quote_prefix() {
        // The bubble must not prepend a `> ` blockquote prefix: that
        // makes the markdown renderer draw a `│ ` border glyph the
        // user can't exclude when copying the prompt back out. The
        // bubble relies entirely on the background tint, so no `│`
        // should appear in the rendered output.
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
