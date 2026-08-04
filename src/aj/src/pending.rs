//! Pending-message box rendered directly above the editor.
//!
//! Shows the one message the user has queued for the viewed agent
//! while it is busy, previewed as a user-message bubble so it reads
//! the same as the message will once it is sent. A hint line at the
//! top of the bubble says when it will be delivered and how to edit
//! or escalate it; steering and follow-up are distinguished by that
//! hint.
//!
//! The queue it previews comes off the [`ChatState`], which every client
//! keeps from `QueueUpdate` frames and the queue read (spec 6.7). Reading
//! the model rather than a live [`aj_agent::queue::MessageQueues`] handle is
//! what makes the box work for a remote frontend, which has no such handle.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::LazyLock;

use aj_agent::events::AgentId;
use aj_agent::message::{AgentMessage, AgentMessageKind};
use aj_agent::queue::PendingKind;
use aj_app::chat::ChatState;
use aj_models::types::{Message, UserContent};
use vaxis::cell::Style;
use vaxis::vxfw::{DrawContext, Event, EventContext, Size, Surface, TextSpan, Widget};

use crate::bubble::Bubble;
use crate::transcript::TranscriptStyles;

/// Message rows shown before the remainder collapses into a
/// `+N more lines` indicator. Keeps a long queued draft from pushing
/// the editor off-screen.
const MAX_BODY_LINES: usize = 6;

/// Display label of the edit gesture in the hint line: the up key, which
/// recalls the queued message into the editor for editing when the editor is
/// empty.
///
/// Plain Up (and Ctrl+P) is a capture-phase keymap binding that fires the same
/// `AjAction::Dequeue` as the `alt+up` chord, but under a stricter gate: only
/// an empty editor with a message pending recalls (see `can_recall_pending`).
/// We resolve the label from the canonical `"up"` chord rather than from the
/// dequeue action, because the action's default chord is `alt+up` and the hint
/// names the single concise key the user actually presses to recall. The
/// editor's own chord table ([`vaxis::vxfw::TextArea::bindings`]) documents the
/// keystroke with a verbose help-screen label (`↑ / Ctrl-P`) that overflows an
/// inline hint, so we render the concise `Up`. Resolving the label through
/// `format_keybinding` keeps one formatting source, so the spelling can't drift
/// from a raw literal.
static EDIT_KEY_LABEL: LazyLock<String> =
    LazyLock::new(|| aj_app::keybindings::format_keybinding("up"));

/// Display label of the steer chord, resolved from the shared binding data, so
/// it reflects a user `[keybindings]` override. Cached on first render, which
/// is after the overrides are installed at startup.
static STEER_KEY_LABEL: LazyLock<String> = LazyLock::new(|| {
    aj_app::keybindings::action_shortcut(aj_app::keybindings::ACTION_SUBMIT_STEERING)
        .expect("aj.message.steer has a default chord")
});

/// Box above the editor previewing the viewed agent's pending message.
pub(crate) struct PendingBox {
    chat: Rc<RefCell<ChatState>>,
    styles: Rc<TranscriptStyles>,
}

/// The pending message a client shows for `agent`: its kind and the
/// coalesced text, or `None` when nothing is queued for it.
///
/// Steering outranks a follow-up, mirroring the queue's own precedence: an
/// agent holds at most one pending message, and which of the two vectors
/// carries it is what names the kind. The texts of several queued messages
/// join with newlines the way the queue coalesces them.
pub(crate) fn pending_message(chat: &ChatState, agent: AgentId) -> Option<(PendingKind, String)> {
    let queue = chat
        .queue()
        .queues
        .iter()
        .find(|queue| queue.agent_id == agent)?;
    let (kind, messages) = if !queue.steering.is_empty() {
        (PendingKind::Steering, &queue.steering)
    } else if !queue.follow_up.is_empty() {
        (PendingKind::FollowUp, &queue.follow_up)
    } else {
        return None;
    };
    let text = messages
        .iter()
        .map(queued_text)
        .collect::<Vec<String>>()
        .join("\n");
    Some((kind, text))
}

/// The text of one queued message. The queues only ever hold user text, so
/// anything else reads as empty rather than as a preview of something the
/// user never typed.
fn queued_text(message: &AgentMessage) -> String {
    let AgentMessageKind::Wire(Message::User(user)) = &message.kind else {
        return String::new();
    };
    user.content
        .iter()
        .filter_map(|block| match block {
            UserContent::Text(text) => Some(text.text.as_str()),
            UserContent::Image(_) => None,
        })
        .collect()
}

impl PendingBox {
    pub(crate) fn new(chat: Rc<RefCell<ChatState>>, styles: Rc<TranscriptStyles>) -> PendingBox {
        PendingBox { chat, styles }
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
                        "  \u{2022}  sends when the turn ends  \u{2022}  {edit} to edit  \u{2022}  {steer} to steer",
                        edit = EDIT_KEY_LABEL.as_str(),
                        steer = STEER_KEY_LABEL.as_str(),
                    ),
                    self.styles.dim,
                ),
            ],
            PendingKind::Steering => vec![
                span("steering".to_string(), self.styles.accent),
                span(
                    format!(
                        "  \u{2022}  sends at the next tool call  \u{2022}  {edit} to edit",
                        edit = EDIT_KEY_LABEL.as_str(),
                    ),
                    self.styles.dim,
                ),
            ],
        }
    }
}

impl Widget for PendingBox {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let pending = {
            let chat = self.chat.borrow();
            let active = chat.active_view();
            pending_message(&chat, active)
        };
        let Some((kind, text)) = pending else {
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
        let lines: Vec<&str> = text.split('\n').collect();
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
            border: None,
            image: None,
        }
        .draw(ctx)
    }

    fn handle_event(&mut self, _ctx: &mut EventContext, _event: &Event) {}
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_agent::events::AgentSettings;
    use aj_app::theme::Theme;
    use aj_wire::AgentQueue;

    use super::*;

    fn chat() -> Rc<RefCell<ChatState>> {
        Rc::new(RefCell::new(ChatState::new(
            AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted".into(),
                thinking: "off".into(),
                thinking_display: "default".into(),
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
            crate::terminal::TerminalCaps::default(),
        ))
    }

    fn pending() -> (PendingBox, Rc<RefCell<ChatState>>) {
        let chat = chat();
        let b = PendingBox::new(Rc::clone(&chat), styles());
        (b, chat)
    }

    /// Note one agent's queue the way a `QueueUpdate` frame does.
    fn note(chat: &Rc<RefCell<ChatState>>, agent: AgentId, steering: &[&str], follow_up: &[&str]) {
        let messages = |texts: &[&str]| {
            texts
                .iter()
                .map(|text| {
                    AgentMessage::wire(Message::User(aj_models::types::UserMessage::text(
                        (*text).to_string(),
                    )))
                })
                .collect()
        };
        chat.borrow_mut().note_queue(AgentQueue {
            agent_id: agent,
            steering: messages(steering),
            follow_up: messages(follow_up),
        });
    }

    fn draw(b: &mut PendingBox, width: u16) -> Surface {
        b.draw(&crate::test_support::draw_ctx(width, None))
    }

    #[test]
    fn empty_queue_takes_zero_height() {
        let (mut b, _chat) = pending();
        assert_eq!(draw(&mut b, 80).size.height, 0);
    }

    /// A drained agent keeps its (now empty) queue entry, so an empty
    /// snapshot must read as "nothing pending" rather than as a kind.
    #[test]
    fn a_drained_queue_takes_zero_height() {
        let (mut b, chat) = pending();
        note(&chat, AgentId::Main, &[], &["do the thing"]);
        assert!(draw(&mut b, 80).size.height > 0);
        note(&chat, AgentId::Main, &[], &[]);
        assert_eq!(draw(&mut b, 80).size.height, 0);
    }

    #[test]
    fn follow_up_shows_hint_above_the_tinted_body() {
        let (mut b, chat) = pending();
        note(&chat, AgentId::Main, &[], &["do the thing"]);
        let surface = draw(&mut b, 80);
        let r = crate::test_support::rows(&surface);
        // Padding row, hint, blank separator, body, padding row.
        assert_eq!(r.len(), 5, "{r:?}");
        assert_eq!(
            r[1],
            format!(
                " queued  •  sends when the turn ends  •  {edit} to edit  •  {steer} to steer",
                edit = EDIT_KEY_LABEL.as_str(),
                steer = STEER_KEY_LABEL.as_str(),
            ),
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

    /// Steering outranks a follow-up queued for the same agent, and its hint
    /// omits the escalation gesture.
    #[test]
    fn steering_hint_omits_the_escalation_gesture() {
        let (mut b, chat) = pending();
        note(&chat, AgentId::Main, &["now"], &["later"]);
        let r = crate::test_support::rows(&draw(&mut b, 80));
        assert_eq!(
            r[1],
            format!(
                " steering  •  sends at the next tool call  •  {edit} to edit",
                edit = EDIT_KEY_LABEL.as_str(),
            ),
        );
        assert_eq!(r[3], " now", "{r:?}");
        assert!(!r.join("\n").contains(STEER_KEY_LABEL.as_str()), "{r:?}");
    }

    /// Several queued messages coalesce into one preview, newline-joined the
    /// way the queue itself joins them.
    #[test]
    fn queued_messages_coalesce_into_one_body() {
        let (mut b, chat) = pending();
        note(&chat, AgentId::Main, &[], &["first", "second"]);
        let r = crate::test_support::rows(&draw(&mut b, 80));
        assert_eq!(r[3], " first", "{r:?}");
        assert_eq!(r[4], " second", "{r:?}");
    }

    #[test]
    fn long_body_collapses_into_overflow_indicator() {
        let (mut b, chat) = pending();
        let text = (1..=10)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        note(&chat, AgentId::Main, &[], &[text.as_str()]);
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
        let (mut b, chat) = pending();
        let wide = "x".repeat(200);
        note(&chat, AgentId::Main, &[], &[wide.as_str()]);
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
        let (mut b, chat) = pending();
        note(&chat, AgentId::Sub(1), &[], &["for the sub"]);
        assert_eq!(draw(&mut b, 80).size.height, 0);
    }
}
