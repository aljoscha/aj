//! Pending-message box rendered directly above the editor.
//!
//! Shows the one message the user has queued for the viewed agent
//! while it is busy, with a hint line describing when it will be sent
//! and how to edit or escalate it. Steering and follow-up are
//! distinguished by the hint and a colored bar.
//!
//! The event pump owns one instance per session and pushes the active
//! view's [`QueueSnapshot`] into it via [`PendingMessage::set_snapshot`]
//! (see the pump's `sync_pending`). Like [`super::loader_status`], the
//! component is attached permanently to its slot and renders an empty
//! line list when nothing is pending, so the slot collapses to zero
//! height between messages.

use std::any::Any;

use aj_agent::queue::{PendingKind, QueueSnapshot};
use aj_tui::ansi::truncate_to_width;
use aj_tui::component::Component;
use aj_tui::keys::InputEvent;
use aj_tui::style;

/// Message rows shown before the remainder collapses into a
/// `+N more lines` indicator. Keeps a long queued draft from pushing
/// the editor off-screen.
const MAX_BODY_LINES: usize = 6;

/// The left gutter marking the queued block, two columns wide.
const BAR: &str = "│ ";

/// Box above the editor showing the viewed agent's pending message.
#[derive(Default)]
pub struct PendingMessage {
    snapshot: QueueSnapshot,
}

impl PendingMessage {
    pub fn new() -> Self {
        Self::default()
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

        let mut out = vec![truncate_to_width(&header(kind), width, "…", false)];

        let body_width = width.saturating_sub(BAR.len()).max(1);
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
        for line in shown {
            out.push(format!(
                "{}{}",
                style::cyan(BAR),
                truncate_to_width(line, body_width, "…", false)
            ));
        }
        if overflow > 0 {
            out.push(format!(
                "{}{}",
                style::cyan(BAR),
                style::dim(&format!("+{overflow} more lines"))
            ));
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

    fn snap(kind: PendingKind, text: &str) -> QueueSnapshot {
        QueueSnapshot {
            kind: Some(kind),
            text: text.to_string(),
        }
    }

    #[test]
    fn empty_snapshot_renders_nothing() {
        let mut c = PendingMessage::new();
        assert!(c.render(80).is_empty());
    }

    #[test]
    fn follow_up_renders_header_and_body() {
        let mut c = PendingMessage::new();
        c.set_snapshot(snap(PendingKind::FollowUp, "do the thing"));
        let lines = c.render(80);
        assert_eq!(lines.len(), 2, "header + one body line");
        assert!(lines[0].contains("queued"));
        assert!(lines[0].contains("alt+enter"));
        assert!(lines[1].contains("do the thing"));
    }

    #[test]
    fn steering_header_omits_escalation_hint() {
        let mut c = PendingMessage::new();
        c.set_snapshot(snap(PendingKind::Steering, "now"));
        let header = &c.render(80)[0];
        assert!(header.contains("steering"));
        assert!(!header.contains("alt+enter"));
    }

    #[test]
    fn long_body_collapses_into_overflow_indicator() {
        let mut c = PendingMessage::new();
        let text = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        c.set_snapshot(snap(PendingKind::FollowUp, &text));
        let lines = c.render(80);
        // header + (MAX_BODY_LINES - 1) body rows + 1 overflow row.
        assert_eq!(lines.len(), 1 + (MAX_BODY_LINES - 1) + 1);
        let last = lines.last().unwrap();
        // 10 lines, 5 shown, 5 collapsed.
        assert!(last.contains("+5 more lines"), "got: {last:?}");
    }
}
