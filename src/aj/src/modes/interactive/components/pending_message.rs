//! Pending-message box rendered directly above the editor.
//!
//! Shows the one message the user has queued for the viewed agent
//! while it is busy, previewed as a user-message bubble so it reads
//! the same as the message will once it is sent. A hint line at the
//! top of the bubble says when it will be delivered and how to edit
//! or escalate it; steering and follow-up are distinguished by that
//! hint.
//!
//! The event pump owns one instance per session and pushes the active
//! view's [`QueueSnapshot`] into it via [`PendingMessage::set_snapshot`]
//! (see the pump's `sync_pending`). Like [`super::loader_status`], the
//! component is attached permanently to its slot and renders an empty
//! line list when nothing is pending, so the slot collapses to zero
//! height between messages.

use std::any::Any;
use std::sync::Arc;

use aj_agent::queue::{PendingKind, QueueSnapshot};
use aj_tui::ansi::{apply_background_to_line, truncate_to_width};
use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::style;

use crate::config::theme::ChatTheme;

/// Message rows shown before the remainder collapses into a
/// `+N more lines` indicator. Keeps a long queued draft from pushing
/// the editor off-screen.
const MAX_BODY_LINES: usize = 6;

/// Horizontal inset, matching the user-message bubble: one column on
/// each side so the queued preview lines up with a sent message.
const PADDING_X: usize = 1;

/// Blank tinted rows above and below the content, matching the
/// user-message bubble's vertical padding.
const PADDING_Y: usize = 1;

/// Box above the editor previewing the viewed agent's pending message.
pub struct PendingMessage {
    snapshot: QueueSnapshot,
    /// Background-paint closure shared with the user-message bubble.
    /// Resolves through the live theme handle, so a hot-reload reskins
    /// the preview without rebuilding the component.
    bg: Arc<dyn Fn(&str) -> String>,
}

impl PendingMessage {
    pub fn new(theme: &ChatTheme) -> Self {
        Self {
            snapshot: QueueSnapshot::default(),
            bg: Arc::clone(&theme.user_message_bg),
        }
    }

    /// Replace the displayed message. A snapshot with `kind: None`
    /// (nothing pending) hides the box on the next render.
    pub fn set_snapshot(&mut self, snapshot: QueueSnapshot) {
        self.snapshot = snapshot;
    }
}

/// Hint line describing the pending message's kind and the gestures
/// that act on it.
fn header(kind: PendingKind) -> String {
    match kind {
        PendingKind::FollowUp => format!(
            "{}{}",
            style::cyan("queued"),
            style::dim(" · sends when the turn ends · ↑ to edit · alt+enter to steer"),
        ),
        PendingKind::Steering => format!(
            "{}{}",
            style::cyan("steering"),
            style::dim(" · sends at the next tool call · ↑ to edit"),
        ),
    }
}

impl Component for PendingMessage {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let Some(kind) = self.snapshot.kind else {
            // Nothing pending → no rows; the slot collapses to zero
            // height so the editor sits flush under the chat.
            return Vec::new();
        };
        if width == 0 {
            return Vec::new();
        }

        // Hint first, then a blank separator row, then the message body
        // — the user sees how the queued text will read once sent, with
        // the hint above it inside the same bubble.
        let mut content = vec![header(kind), String::new()];
        let lines: Vec<&str> = self.snapshot.text.split('\n').collect();
        // Show every line up to the cap; past it, keep `MAX_BODY_LINES
        // - 1` rows and spend the last on the overflow indicator so the
        // box height is bounded.
        let (shown, overflow) = if lines.len() > MAX_BODY_LINES {
            (
                &lines[..MAX_BODY_LINES - 1],
                lines.len() - (MAX_BODY_LINES - 1),
            )
        } else {
            (&lines[..], 0)
        };
        content.extend(shown.iter().map(|line| line.to_string()));
        if overflow > 0 {
            content.push(style::dim(&format!("+{overflow} more lines")));
        }

        // Paint every content row plus the top/bottom padding rows
        // through the shared user-message background closure, inset by
        // `PADDING_X`, so the whole block reads as one tinted bubble.
        let content_width = width.saturating_sub(PADDING_X * 2).max(1);
        let inset = " ".repeat(PADDING_X);
        let blank = apply_background_to_line("", width, self.bg.as_ref());

        let mut out = Vec::with_capacity(content.len() + PADDING_Y * 2);
        for _ in 0..PADDING_Y {
            out.push(blank.clone());
        }
        for line in content {
            let row = format!(
                "{inset}{}",
                truncate_to_width(&line, content_width, "…", false)
            );
            out.push(apply_background_to_line(&row, width, self.bg.as_ref()));
        }
        for _ in 0..PADDING_Y {
            out.push(blank.clone());
        }
        out
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }
}

impl AsRef<dyn Any> for PendingMessage {
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

    fn snap(kind: PendingKind, text: &str) -> QueueSnapshot {
        QueueSnapshot {
            kind: Some(kind),
            text: text.to_string(),
        }
    }

    /// Index of the first rendered row whose text contains `needle`.
    fn line_with(lines: &[String], needle: &str) -> Option<usize> {
        lines.iter().position(|l| l.contains(needle))
    }

    #[test]
    fn empty_snapshot_renders_nothing() {
        let mut c = PendingMessage::new(&theme());
        assert!(c.render(80).is_empty());
    }

    #[test]
    fn follow_up_shows_hint_above_the_body() {
        let mut c = PendingMessage::new(&theme());
        c.set_snapshot(snap(PendingKind::FollowUp, "do the thing"));
        let lines = c.render(80);
        let hint = line_with(&lines, "queued").expect("hint row present");
        let body = line_with(&lines, "do the thing").expect("body row present");
        assert!(hint < body, "the hint must sit right above the body");
        assert!(lines[hint].contains("alt+enter"));
    }

    #[test]
    fn rows_carry_the_user_message_background_escape() {
        // The preview reuses the user-message bubble's background
        // closure, so its rows must carry a background SGR escape
        // (truecolor `\x1b[48;2;…m` or 256-color `\x1b[48;5;…m`, both
        // sharing the `\x1b[48;` prefix).
        let mut c = PendingMessage::new(&theme());
        c.set_snapshot(snap(PendingKind::FollowUp, "do the thing"));
        let lines = c.render(80);
        assert!(
            lines.iter().any(|l| l.contains("\x1b[48;")),
            "expected at least one tinted row, got: {lines:?}",
        );
    }

    #[test]
    fn steering_header_omits_escalation_hint() {
        let mut c = PendingMessage::new(&theme());
        c.set_snapshot(snap(PendingKind::Steering, "now"));
        let lines = c.render(80);
        let hint = line_with(&lines, "steering").expect("hint row present");
        assert!(!lines[hint].contains("alt+enter"));
    }

    #[test]
    fn long_body_collapses_into_overflow_indicator() {
        let mut c = PendingMessage::new(&theme());
        let text = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        c.set_snapshot(snap(PendingKind::FollowUp, &text));
        let lines = c.render(80);
        // padding + hint + separator + (MAX_BODY_LINES - 1) body rows + overflow + padding.
        assert_eq!(
            lines.len(),
            PADDING_Y + 1 + 1 + (MAX_BODY_LINES - 1) + 1 + PADDING_Y
        );
        // 10 lines, 5 shown, 5 collapsed.
        assert!(
            line_with(&lines, "+5 more lines").is_some(),
            "got: {lines:?}"
        );
    }
}
