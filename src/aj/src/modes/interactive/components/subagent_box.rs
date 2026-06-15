//! Sub-agent box component.
//!
//! Owns one sub-agent's transcript (an inner [`Container`]) and renders
//! it in one of two modes:
//!
//! - **Compact** (the default, used while the main agent's view is
//!   active): an inline gray box with the tool-bubble aesthetic. The
//!   inner transcript is windowed to its tail
//!   ([`SUBAGENT_BOX_COMPACT_ROWS`] rows) so a long-running sub-agent
//!   never floods the scrollback. Tool calls inside render header-only
//!   (no bubble, no body) so they compose cleanly inside this box's own
//!   painted background.
//! - **Full** (used when the user switches to observe this sub-agent):
//!   a single themed header line followed by the whole inner transcript
//!   verbatim, full results, no window. The terminal scrolls. Inner
//!   tool calls render with their full bodies because [`Self::set_mode`]
//!   clears their `header_only` flag.
//!
//! The mode is driven by the owning `ChatView`: the active sub-agent's
//! box is `Full`, every other box is `Compact`.

use std::any::Any;
use std::sync::Arc;

use aj_tui::ansi::{apply_background_to_line, truncate_to_width, wrap_text_with_ansi};
use aj_tui::component::Component;
use aj_tui::container::Container;
use aj_tui::keys::InputEvent;
use aj_tui::style;

use crate::config::theme::ChatTheme;
use crate::modes::interactive::components::tool_execution::ToolExecutionComponent;

/// Inner transcript rows shown in the compact (inline) box before
/// the tail window kicks in; the single knob to tune.
const SUBAGENT_BOX_COMPACT_ROWS: usize = 18;

/// Horizontal padding inside the compact box (one column on each
/// side so the tinted rectangle reads as an inset block).
const PADDING_X: usize = 1;

/// Vertical padding inside the compact box (one bg-painted blank row
/// above and below the content, matching the tool-bubble rhythm).
const PADDING_Y: usize = 1;

/// Minimum total render width at which the box framing kicks in.
/// Below this we fall back to a plain header+body listing so the
/// bg-padding pipeline never paints a degenerate row and the Tui's
/// strict line-width check doesn't trip.
const MIN_BOX_WIDTH: usize = 3;

/// Lifecycle status of the owned sub-agent. Drives the title glyph
/// (dim `▸` running, green `✓` done, red `✗` failed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubAgentStatus {
    Running,
    Done,
    Failed,
}

/// Render mode of the box. `Compact` is the inline windowed box;
/// `Full` is the switched-to whole-transcript view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubAgentBoxMode {
    Compact,
    Full,
}

/// On-screen representation of one sub-agent's transcript.
pub struct SubAgentBox {
    /// The `N` in `Sub(N)`; shown in the title/header.
    agent_index: usize,
    /// Task text from `SubAgentStart`; summarized to one line in the
    /// title.
    task: String,
    /// Current lifecycle status; drives the title glyph.
    status: SubAgentStatus,
    /// This sub-agent's components, in append order.
    inner: Container,
    /// Render mode; `Compact` by default.
    mode: SubAgentBoxMode,
    /// Inner transcript rows shown in compact mode before the tail
    /// window kicks in.
    compact_rows: usize,
    /// Gray bg-paint closure (shared `toolBg`), used to paint the
    /// compact box's rows full-width.
    bg: Arc<dyn Fn(&str) -> String>,
}

impl SubAgentBox {
    /// Build a new box for `Sub(agent_index)` with the given task.
    /// Starts in [`SubAgentStatus::Running`] and [`SubAgentBoxMode::Compact`]
    /// with an empty inner transcript.
    pub fn new(agent_index: usize, task: &str, theme: &ChatTheme) -> Self {
        Self {
            agent_index,
            task: task.to_string(),
            status: SubAgentStatus::Running,
            inner: Container::new(),
            mode: SubAgentBoxMode::Compact,
            compact_rows: SUBAGENT_BOX_COMPACT_ROWS,
            bg: Arc::clone(&theme.tool_pending_bg),
        }
    }

    /// Mutable access to the inner transcript container so the event
    /// pump can route this sub-agent's components into it.
    pub fn inner_mut(&mut self) -> &mut Container {
        &mut self.inner
    }

    pub fn agent_index(&self) -> usize {
        self.agent_index
    }

    pub fn task(&self) -> &str {
        &self.task
    }

    pub fn status(&self) -> SubAgentStatus {
        self.status
    }

    /// Update the lifecycle status, invalidating only on a real change.
    pub fn set_status(&mut self, status: SubAgentStatus) {
        if self.status == status {
            return;
        }
        self.status = status;
        self.invalidate();
    }

    pub fn mode(&self) -> SubAgentBoxMode {
        self.mode
    }

    /// Switch the render mode. On a real change this walks the inner
    /// transcript and flips every [`ToolExecutionComponent`]'s
    /// `header_only` flag to match the mode (header-only in compact,
    /// full bubbles in full), then invalidates.
    pub fn set_mode(&mut self, mode: SubAgentBoxMode) {
        if self.mode == mode {
            return;
        }
        self.mode = mode;
        let header_only = matches!(mode, SubAgentBoxMode::Compact);
        for i in 0..self.inner.len() {
            if let Some(tool) = self.inner.get_mut_as::<ToolExecutionComponent>(i) {
                tool.set_header_only(header_only);
            }
        }
        self.invalidate();
    }

    /// Status glyph for the title/header.
    fn status_glyph(&self) -> String {
        match self.status {
            SubAgentStatus::Running => style::dim("▸"),
            SubAgentStatus::Done => style::green("✓"),
            SubAgentStatus::Failed => style::red("✗"),
        }
    }

    /// Collapse the task text to a single line (newlines/tabs to
    /// spaces) so it never wraps the title.
    fn task_summary(&self) -> String {
        self.task
            .replace(['\n', '\r', '\t'], " ")
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// One-line compact title `{glyph} agent {N} · {task}`, truncated as
    /// a whole to `content_width`.
    ///
    /// The assembled line is truncated rather than just the task summary:
    /// the `{glyph} agent {N} · ` prefix is ~12 columns, so truncating
    /// only the summary leaves the title that much wider than
    /// `content_width` and overflows the box (the bg-paint pipeline pads
    /// but never trims, so the over-wide row reaches the Tui's strict
    /// line-width check and panics). Truncating the whole line also keeps
    /// the contract when the prefix alone would exceed a very narrow box.
    fn compact_title(&self, content_width: usize) -> String {
        let glyph = self.status_glyph();
        let label = style::bold(&format!("agent {}", self.agent_index));
        let summary = style::dim(&self.task_summary());
        let title = format!("{glyph} {label} · {summary}");
        truncate_to_width(&title, content_width, "…", false)
    }
}

impl Component for SubAgentBox {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Degenerate width: plain fallback so the bg-padding pipeline
        // never paints a too-narrow row. Mirrors the tool bubble's
        // degraded path.
        if width < MIN_BOX_WIDTH {
            let mut out = Vec::new();
            out.extend(wrap_text_with_ansi(
                &self.compact_title(width.max(1)),
                width.max(1),
            ));
            let inner_lines = self.inner.render(width.max(1));
            let body: Vec<String> = match self.mode {
                SubAgentBoxMode::Compact => {
                    let start = inner_lines.len().saturating_sub(self.compact_rows);
                    inner_lines[start..].to_vec()
                }
                SubAgentBoxMode::Full => inner_lines,
            };
            for line in &body {
                out.extend(wrap_text_with_ansi(line, width.max(1)));
            }
            return out;
        }

        // Full mode: one themed header line, then the whole inner
        // transcript verbatim. No bg, no window; the terminal scrolls.
        if matches!(self.mode, SubAgentBoxMode::Full) {
            let header = style::dim(&truncate_to_width(
                &format!(
                    "agent {} · {} — observing",
                    self.agent_index,
                    self.task_summary()
                ),
                width,
                "…",
                false,
            ));
            let mut out = vec![header];
            out.extend(self.inner.render(width));
            return out;
        }

        // Compact mode: inline gray box with a tail window.
        let content_width = width.saturating_sub(2 * PADDING_X).max(1);
        let inner_lines = self.inner.render(content_width);

        // Window to the tail, reserving one row for the dropped-lines
        // hint so the total body never exceeds `compact_rows`.
        let body: Vec<String> = if inner_lines.len() > self.compact_rows {
            let keep = self.compact_rows.saturating_sub(1).max(1);
            let earlier = inner_lines.len() - keep;
            let mut windowed = Vec::with_capacity(keep + 1);
            windowed.push(style::dim(&format!("… ({earlier} earlier lines)")));
            windowed.extend_from_slice(&inner_lines[inner_lines.len() - keep..]);
            windowed
        } else {
            inner_lines
        };

        let mut content_lines = Vec::with_capacity(body.len() + 1);
        content_lines.push(self.compact_title(content_width));
        content_lines.extend(body);

        let padding = " ".repeat(PADDING_X);
        let mut out = Vec::with_capacity(content_lines.len() + 2 * PADDING_Y);
        for _ in 0..PADDING_Y {
            out.push(apply_background_to_line("", width, self.bg.as_ref()));
        }
        for line in &content_lines {
            let padded = format!("{padding}{line}");
            out.push(apply_background_to_line(&padded, width, self.bg.as_ref()));
        }
        for _ in 0..PADDING_Y {
            out.push(apply_background_to_line("", width, self.bg.as_ref()));
        }
        out
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }

    fn invalidate(&mut self) {
        self.inner.invalidate();
    }
}

impl AsRef<dyn Any> for SubAgentBox {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_tui::ansi::visible_width;
    use aj_tui::components::text::Text;

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
        String::from_utf8(out).expect("strip_ansi: surviving bytes remain valid UTF-8")
    }

    fn push_text(b: &mut SubAgentBox, text: &str) {
        b.inner_mut().add_child(Box::new(Text::new(text, 0, 0)));
    }

    #[test]
    fn compact_render_is_full_width_with_painted_pads() {
        let mut b = SubAgentBox::new(0, "summarize the repo", &theme());
        push_text(&mut b, "hello");
        push_text(&mut b, "world");
        let width = 60;
        let lines = b.render(width);

        // First and last rows are bg-painted blank pads.
        assert!(strip_ansi(&lines[0]).trim().is_empty());
        assert!(
            strip_ansi(lines.last().expect("non-empty render"))
                .trim()
                .is_empty()
        );

        // A middle row carries the title with the agent label + task.
        let joined: String = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(joined.contains("agent 0"), "{joined:?}");
        assert!(joined.contains("summarize the repo"), "{joined:?}");

        // Every returned row is exactly `width` wide.
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(visible_width(line), width, "row {i} width: {line:?}");
        }
    }

    #[test]
    fn compact_render_windows_to_the_tail_with_a_hint() {
        let mut b = SubAgentBox::new(0, "task", &theme());
        for i in 0..40 {
            push_text(&mut b, &format!("line {i}"));
        }
        let lines = b.render(60);
        let joined: String = lines.iter().map(|l| strip_ansi(l)).collect();

        // Most recent lines survive and the dropped-lines hint shows.
        assert!(joined.contains("line 39"), "{joined:?}");
        assert!(joined.contains("earlier lines"), "{joined:?}");

        // Body windowed to compact_rows; plus title + 2 pads + slack.
        assert!(
            lines.len() <= SUBAGENT_BOX_COMPACT_ROWS + 4,
            "too many rows: {}",
            lines.len()
        );
    }

    #[test]
    fn compact_render_drops_the_earliest_line() {
        let mut b = SubAgentBox::new(0, "task", &theme());
        for i in 0..40 {
            push_text(&mut b, &format!("entry-{i}-marker"));
        }
        let lines = b.render(60);
        let joined: String = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(joined.contains("entry-39-marker"));
        assert!(!joined.contains("entry-0-marker"), "{joined:?}");
    }

    #[test]
    fn done_status_shows_a_check_glyph_in_the_title() {
        let mut b = SubAgentBox::new(0, "task", &theme());
        push_text(&mut b, "hi");
        b.set_status(SubAgentStatus::Done);
        let lines = b.render(60);
        let joined: String = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(joined.contains("✓"), "{joined:?}");
    }

    #[test]
    fn full_mode_renders_header_then_inner_lines() {
        let mut b = SubAgentBox::new(0, "explore", &theme());
        push_text(&mut b, "first inner line");
        push_text(&mut b, "second inner line");
        b.set_mode(SubAgentBoxMode::Full);
        let width = 60;
        let lines = b.render(width);

        let header = strip_ansi(&lines[0]);
        assert!(header.contains("agent 0"), "{header:?}");
        assert!(header.contains("observing"), "{header:?}");

        let joined: String = lines.iter().map(|l| strip_ansi(l)).collect();
        assert!(joined.contains("first inner line"));
        assert!(joined.contains("second inner line"));

        for (i, line) in lines.iter().enumerate() {
            assert!(
                visible_width(line) <= width,
                "row {i} exceeds width: {line:?}"
            );
        }
    }

    #[test]
    fn compact_title_with_a_long_task_never_overflows_the_box() {
        // Regression: `compact_title` used to truncate only the task
        // summary to `content_width`, then prepend the
        // `{glyph} agent {N} · ` prefix — so the title overran the box
        // by the prefix width. The bg-paint pads but never trims, so
        // the over-wide row reached the Tui's strict line-width check
        // and panicked. Render a far-too-long task into a wide box and
        // assert every painted row is exactly `width`.
        let long_task = "Search the aj codebase (Rust workspace at /home/ubuntu/aj) for \
            patterns where we iterate over chat messages and call setters to update \
            rendering settings. Specifically look for thinking visibility and tool \
            result expansion toggles."
            .to_string();
        let mut b = SubAgentBox::new(1, &long_task, &theme());
        b.set_status(SubAgentStatus::Done);
        push_text(&mut b, "inner line");
        let width = 202;
        let lines = b.render(width);
        for (i, line) in lines.iter().enumerate() {
            assert_eq!(visible_width(line), width, "row {i} width: {line:?}");
        }
    }

    #[test]
    fn degenerate_width_does_not_panic_and_respects_width() {
        let mut b = SubAgentBox::new(0, "task", &theme());
        for i in 0..30 {
            push_text(&mut b, &format!("line {i}"));
        }
        let width = 2;
        let lines = b.render(width);
        for (i, line) in lines.iter().enumerate() {
            assert!(
                visible_width(line) <= width,
                "row {i} exceeds width: {line:?}"
            );
        }
    }
}
