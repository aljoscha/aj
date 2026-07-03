//! Pending-message box rendered directly above the editor.
//!
//! Shows the one message the user has queued for the viewed agent
//! while it is busy, previewed as a user-message bubble so it reads
//! the same as the message will once it is sent. A hint line at the
//! top of the bubble says when it will be delivered and how to edit
//! or escalate it; steering and follow-up are distinguished by that
//! hint.
//!
//! The widget re-reads the live [`MessageQueues`] snapshot for the
//! active view on every draw, so it can never trust a stale event
//! payload. `QueueUpdate` events reduce to a redraw ping, which is
//! all the sync this needs.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;

use aj_agent::queue::{MessageQueues, PendingKind};
use aj_app::chat::ChatState;
use vaxis::cell::Style;
use vaxis::vxfw::{DrawContext, Event, EventContext, Size, Surface, TextSpan, Widget};

use crate::bubble::Bubble;
use crate::transcript::TranscriptStyles;

/// Message rows shown before the remainder collapses into a
/// `+N more lines` indicator. Keeps a long queued draft from pushing
/// the editor off-screen.
const MAX_BODY_LINES: usize = 6;

/// Display label of the edit gesture in the hint line. The plain Up
/// arrow is an editor-level convention (`tui.editor.cursorUp` in aj's
/// vocabulary), not an `aj.*` action, so it is spelled here.
const EDIT_KEY_LABEL: &str = "Up";

/// Display label of the steer chord, resolved from the shared default
/// binding table. Follows user `[keybindings]` overrides once those
/// land.
static STEER_KEY_LABEL: LazyLock<String> = LazyLock::new(|| {
    aj_app::keybindings::default_action_shortcut(aj_app::keybindings::ACTION_SUBMIT_STEERING)
        .expect("aj.message.steer has a default chord")
});

/// Box above the editor previewing the viewed agent's pending message.
pub(crate) struct PendingBox {
    chat: Rc<RefCell<ChatState>>,
    /// Shared queue handle, cloned from the session core at
    /// construction.
    queues: MessageQueues,
    styles: Rc<TranscriptStyles>,
}

impl PendingBox {
    pub(crate) fn new(
        chat: Rc<RefCell<ChatState>>,
        queues: MessageQueues,
        styles: Rc<TranscriptStyles>,
    ) -> PendingBox {
        PendingBox {
            chat,
            queues,
            styles,
        }
    }

    /// Replace the palette styles, for a runtime theme swap.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        self.styles = styles;
    }

    /// Hint spans describing the pending message's kind and the
    /// gestures that act on it, mirroring `aj`'s wording.
    fn hint(&self, kind: PendingKind) -> Vec<TextSpan> {
        let span = |text: String, style: Style| TextSpan {
            text,
            style,
            ..TextSpan::default()
        };
        match kind {
            PendingKind::FollowUp => vec![
                span("queued".to_string(), self.styles.accent),
                span(
                    format!(
                        "  \u{2022}  sends when the turn ends  \u{2022}  {EDIT_KEY_LABEL} to edit  \u{2022}  {steer} to steer",
                        steer = STEER_KEY_LABEL.as_str(),
                    ),
                    self.styles.dim,
                ),
            ],
            PendingKind::Steering => vec![
                span("steering".to_string(), self.styles.accent),
                span(
                    format!(
                        "  \u{2022}  sends at the next tool call  \u{2022}  {EDIT_KEY_LABEL} to edit"
                    ),
                    self.styles.dim,
                ),
            ],
        }
    }
}

impl Widget for PendingBox {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let active = self.chat.borrow().active_view();
        let snapshot = self.queues.snapshot(active);
        let Some(kind) = snapshot.kind else {
            // Nothing pending: no rows, the slot collapses to zero
            // height so the editor sits flush under the chat.
            return Surface::with_size(Size {
                width: ctx.max.width.unwrap_or(0),
                height: 0,
            });
        };
        let span = |text: String, style: Style| TextSpan {
            text,
            style,
            ..TextSpan::default()
        };

        // Hint first, then a blank separator row, then the message
        // body, so the user sees how the queued text will read once
        // sent, with the hint above it inside the same bubble.
        let mut spans = self.hint(kind);
        spans.push(span("\n\n".to_string(), self.styles.user));
        let lines: Vec<&str> = snapshot.text.split('\n').collect();
        // Show every line up to the cap; past it, keep
        // `MAX_BODY_LINES - 1` rows and spend the last on the
        // overflow indicator so the box height is bounded.
        let (shown, overflow) = if lines.len() > MAX_BODY_LINES {
            (
                &lines[..MAX_BODY_LINES - 1],
                lines.len() - (MAX_BODY_LINES - 1),
            )
        } else {
            (&lines[..], 0)
        };
        for (i, line) in shown.iter().enumerate() {
            if i > 0 {
                spans.push(span("\n".to_string(), self.styles.user));
            }
            spans.push(span((*line).to_string(), self.styles.user));
        }
        if overflow > 0 {
            spans.push(span("\n".to_string(), self.styles.user));
            spans.push(span(format!("+{overflow} more lines"), self.styles.dim));
        }

        // The user-message tint marks the preview as "yours"; rows
        // truncate rather than wrap so a wide draft can't grow the
        // box, and the box sits flush above the editor (no spacer).
        Bubble {
            text: spans,
            bg: Some(self.styles.user_message_bg),
            base: self.styles.text,
            softwrap: false,
            trailing_spacer: false,
        }
        .draw(ctx)
    }

    fn handle_event(&mut self, _ctx: &mut EventContext, _event: &Event) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_agent::events::{AgentId, AgentSettings};
    use aj_app::theme::Theme;

    use super::*;

    fn chat() -> Rc<RefCell<ChatState>> {
        Rc::new(RefCell::new(ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                speed: "standard".into(),
                verbosity: "default".into(),
            },
            0,
            Arc::new(Vec::new()),
        )))
    }

    fn styles() -> Rc<TranscriptStyles> {
        Rc::new(TranscriptStyles::from_theme(
            &Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
        ))
    }

    fn pending() -> (PendingBox, MessageQueues) {
        let queues = MessageQueues::default();
        let b = PendingBox::new(chat(), queues.clone(), styles());
        (b, queues)
    }

    fn draw(b: &mut PendingBox, width: u16) -> Surface {
        b.draw(&crate::test_support::draw_ctx(width, None))
    }

    #[test]
    fn empty_queue_takes_zero_height() {
        let (mut b, _q) = pending();
        assert_eq!(draw(&mut b, 80).size.height, 0);
    }

    #[test]
    fn follow_up_shows_hint_above_the_tinted_body() {
        let (mut b, q) = pending();
        q.append_follow_up(AgentId::Main, "do the thing");
        let surface = draw(&mut b, 80);
        let r = crate::test_support::rows(&surface);
        // Padding row, hint, blank separator, body, padding row.
        assert_eq!(r.len(), 5, "{r:?}");
        assert_eq!(
            r[1],
            " queued  •  sends when the turn ends  •  Up to edit  •  Alt+Enter to steer",
        );
        assert_eq!(r[2], "");
        assert_eq!(r[3], " do the thing");
        // Every row carries the user-message tint (no untinted
        // trailing spacer: the box sits flush above the editor).
        let s = styles();
        let grid = crate::test_support::flatten(&surface);
        for row in &grid {
            for cell in row {
                assert_eq!(cell.style.bg, s.user_message_bg);
            }
        }
        // The kind label is accent-colored.
        assert_eq!(grid[1][1].style.fg, s.accent.fg);
    }

    #[test]
    fn steering_hint_omits_the_escalation_gesture() {
        let (mut b, q) = pending();
        q.append_steering(AgentId::Main, "now");
        let r = crate::test_support::rows(&draw(&mut b, 80));
        assert_eq!(
            r[1],
            " steering  •  sends at the next tool call  •  Up to edit",
        );
        assert!(!r.join("\n").contains("Alt+Enter"), "{r:?}");
    }

    #[test]
    fn long_body_collapses_into_overflow_indicator() {
        let (mut b, q) = pending();
        let text = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        q.append_follow_up(AgentId::Main, &text);
        let r = crate::test_support::rows(&draw(&mut b, 80));
        // Padding + hint + separator + (MAX_BODY_LINES - 1) body rows
        // + overflow + padding.
        assert_eq!(r.len(), 1 + 1 + 1 + (MAX_BODY_LINES - 1) + 1 + 1, "{r:?}");
        assert!(r.join("\n").contains("+5 more lines"), "{r:?}");
        assert!(!r.join("\n").contains("line 6"), "{r:?}");
    }

    /// Wide content truncates with an ellipsis instead of wrapping,
    /// so the box height only depends on the line count.
    #[test]
    fn wide_lines_truncate_instead_of_wrapping() {
        let (mut b, q) = pending();
        q.append_follow_up(AgentId::Main, &"x".repeat(200));
        let surface = draw(&mut b, 40);
        let r = crate::test_support::rows(&surface);
        assert_eq!(r.len(), 5, "hint + separator + one body row: {r:?}");
        let grid = crate::test_support::flatten(&surface);
        // Both the (long) hint row and the body row end in the
        // ellipsis at the content edge.
        assert_eq!(grid[1][38].char.grapheme(), "…", "{r:?}");
        assert_eq!(grid[3][38].char.grapheme(), "…", "{r:?}");
    }

    /// The preview only shows the viewed agent's queue.
    #[test]
    fn other_agents_queues_stay_hidden() {
        let (mut b, q) = pending();
        q.append_follow_up(AgentId::Sub(1), "for the sub");
        assert_eq!(draw(&mut b, 80).size.height, 0);
    }
}
