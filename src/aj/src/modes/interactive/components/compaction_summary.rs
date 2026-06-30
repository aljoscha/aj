//! Compaction-summary component.
//!
//! Renders the durable record of a context compaction as a single
//! collapsible scrollback row: a one-line header stating how much
//! context was freed, expandable to show the generated summary in
//! full.
//!
//! Folding reuses the session-wide `tools_expanded` setting — the same
//! flag tool bodies honour. A compaction summary is long-form,
//! tool-like output, so it expands and collapses together with tool
//! results under one keystroke (`aj.tools.expand`) rather than
//! introducing a separate toggle. The component holds a clone of the
//! shared [`RenderSettings`] and reconciles on the generation counter
//! at render time, exactly like
//! [`ToolExecutionComponent`](super::tool_execution::ToolExecutionComponent).

use std::any::Any;

use aj_tui::ansi::wrap_text_with_ansi;
use aj_tui::component::Component;
use aj_tui::components::markdown::{Markdown, MarkdownTheme};
use aj_tui::keys::InputEvent;
use aj_tui::style;

use crate::config::theme::ChatTheme;
use crate::modes::interactive::components::footer::format_tokens;
use crate::modes::interactive::render_settings::RenderSettings;

/// One column of horizontal inset so the row aligns with the inset
/// content of bubble components and notices.
const PADDING_X: usize = 1;
/// No internal vertical padding; the chat container's auto-spacer
/// supplies the gap between scrollback children.
const PADDING_Y: usize = 0;

/// On-screen representation of one compaction checkpoint.
pub struct CompactionSummaryComponent {
    /// One-line header (token delta + freed percentage), without the
    /// trailing expand hint. The hint is appended at render time so a
    /// keybindings change is reflected without rebuilding the line.
    header: String,
    /// The generated compaction summary (markdown). Kept as the source
    /// of truth so the markdown widget can be dropped while collapsed
    /// and rebuilt on a later expand.
    summary: String,
    markdown_theme: MarkdownTheme,
    /// Shared session-wide render settings; read at render time so an
    /// `aj.tools.expand` flip reaches this component without the toggle
    /// site walking the transcript.
    settings: RenderSettings,
    /// Generation of [`Self::settings`] the cached widget reflects.
    last_generation: u64,
    /// Last-applied expansion mode (mirrors `settings.tools_expanded()`).
    expanded: bool,
    /// Lazily built markdown widget for the summary; present only while
    /// expanded with a non-empty summary.
    widget: Option<Markdown>,
}

impl CompactionSummaryComponent {
    /// Build a component for a finished compaction. `tokens_before` /
    /// `tokens_after` are the estimated occupancy on either side of the
    /// checkpoint; `summary` is the generated text shown when expanded.
    pub fn new(
        tokens_before: u64,
        tokens_after: u64,
        summary: &str,
        chat_theme: &ChatTheme,
        settings: RenderSettings,
    ) -> Self {
        let expanded = settings.tools_expanded();
        let last_generation = settings.generation();
        let mut me = Self {
            header: header_line(tokens_before, tokens_after),
            summary: summary.to_string(),
            markdown_theme: chat_theme.markdown.clone(),
            settings,
            last_generation,
            expanded,
            widget: None,
        };
        me.refresh_widget();
        me
    }

    /// Pull the shared expansion mode in and rebuild the widget when it
    /// changed. A no-op when the mode is unchanged, so an unrelated
    /// toggle (thinking fold, which shares the generation counter)
    /// doesn't churn the cached widget.
    fn reconcile_settings(&mut self) {
        self.last_generation = self.settings.generation();
        let expanded = self.settings.tools_expanded();
        if self.expanded == expanded {
            return;
        }
        self.expanded = expanded;
        self.refresh_widget();
    }

    /// (Re)build the summary markdown widget when expanded; drop it
    /// when collapsed. The buffer stays the source of truth, so a
    /// later re-expand rebuilds the same content.
    fn refresh_widget(&mut self) {
        if !self.expanded || self.summary.is_empty() {
            self.widget = None;
            return;
        }
        match self.widget.as_mut() {
            Some(md) => md.set_text(&self.summary),
            None => {
                self.widget = Some(Markdown::new(
                    &self.summary,
                    PADDING_X,
                    PADDING_Y,
                    self.markdown_theme.clone(),
                    None,
                ));
            }
        }
    }

    /// The dim header line, with the `(<key> to expand)` hint appended
    /// while collapsed (and only when there's a summary to reveal and
    /// the toggle is bound). Resolves the key through the keybindings
    /// registry the same way the thinking/tool hints do.
    fn styled_header(&self) -> String {
        if self.expanded || self.summary.is_empty() {
            return style::dim(&self.header);
        }
        let kb = aj_tui::keybindings::get();
        let keys = kb.get_keys(crate::config::keybindings::ACTION_TOOLS_EXPAND);
        let header = match keys.first() {
            Some(key) => {
                let key = aj_tui::keybindings::format_keybinding(key);
                format!("{} ({key} to expand)", self.header)
            }
            None => self.header.clone(),
        };
        style::dim(&header)
    }
}

impl Component for CompactionSummaryComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        if self.settings.generation() != self.last_generation {
            self.reconcile_settings();
        }
        let gutter = " ".repeat(PADDING_X.min(width));
        let header = format!("{gutter}{}", self.styled_header());
        let mut lines: Vec<aj_tui::Line> = wrap_text_with_ansi(&header, width.max(1))
            .into_iter()
            .map(aj_tui::Line::from)
            .collect();
        if let Some(w) = self.widget.as_mut() {
            // One blank row between the header and the expanded summary,
            // matching the assistant message's intra-block spacing.
            lines.push(aj_tui::Line::default());
            lines.extend(w.render(width));
        }
        lines
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        if let Some(w) = self.widget.as_mut() {
            w.invalidate();
        }
    }
}

impl AsRef<dyn Any> for CompactionSummaryComponent {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

/// Format the header line: `Context compacted: 152k → 48k tokens
/// (freed 68%)`. `freed` is `0` when occupancy didn't drop (a
/// degenerate compaction), avoiding a misleading negative percentage.
#[allow(clippy::as_conversions)]
fn header_line(tokens_before: u64, tokens_after: u64) -> String {
    let freed = if tokens_before > tokens_after && tokens_before > 0 {
        ((tokens_before - tokens_after) as f64 / tokens_before as f64 * 100.0).round() as u64
    } else {
        0
    };
    format!(
        "Context compacted: {} → {} tokens (freed {freed}%)",
        format_tokens(tokens_before),
        format_tokens(tokens_after),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::theme::{Theme, ThemeHandle, chat_theme};

    fn theme() -> ChatTheme {
        chat_theme(&ThemeHandle::new(Theme::bundled_dark()), true)
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

    /// `RenderSettings` with the given tool-expansion mode (the only
    /// setting this component reads).
    fn settings(tools_expanded: bool) -> RenderSettings {
        RenderSettings::new(false, tools_expanded, true)
    }

    #[test]
    fn header_states_freed_percentage() {
        let mut c = CompactionSummaryComponent::new(
            100_000,
            25_000,
            "the summary",
            &theme(),
            settings(false),
        );
        let joined: String = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(joined.contains("Context compacted"), "{joined:?}");
        assert!(joined.contains("freed 75%"), "{joined:?}");
    }

    #[test]
    fn collapsed_hides_summary_and_advertises_expand_hint() {
        crate::config::keybindings::install_global_manager_defaults();
        let mut c = CompactionSummaryComponent::new(
            100_000,
            25_000,
            "secret summary body",
            &theme(),
            settings(false),
        );
        let lines = c.render(80);
        let joined: String = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(!joined.contains("secret summary body"), "{joined:?}");
        assert!(joined.contains("to expand"), "{joined:?}");
    }

    #[test]
    fn expanded_renders_summary_body() {
        let mut c = CompactionSummaryComponent::new(
            100_000,
            25_000,
            "the full summary body",
            &theme(),
            settings(true),
        );
        let joined: String = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(joined.contains("the full summary body"), "{joined:?}");
        // No expand hint while already expanded.
        assert!(!joined.contains("to expand"), "{joined:?}");
    }

    #[test]
    fn toggling_expansion_reveals_then_hides_the_body() {
        let s = settings(false);
        let mut c =
            CompactionSummaryComponent::new(100_000, 25_000, "body text", &theme(), s.clone());
        assert!(c.widget.is_none());

        s.set_tools_expanded(true);
        let joined: String = c.render(80).iter().map(|l| strip_ansi(l)).collect();
        assert!(joined.contains("body text"), "{joined:?}");
        assert!(c.widget.is_some());

        s.set_tools_expanded(false);
        c.render(80);
        assert!(c.widget.is_none());
    }
}
