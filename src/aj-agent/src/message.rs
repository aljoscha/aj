//! Agent-level transcript entries.
//!
//! `AgentMessage` is the unit of the agent's in-memory transcript and
//! the input shape for `Agent::seed_messages` / output shape for
//! `Agent::messages()`. It wraps wire-level [`Message`]s so the
//! transcript can later be extended with agent-only entries (e.g.
//! UI-only annotations, tool batches, system prompt anchors) without
//! breaking the event protocol.
//!
//! Today there is exactly one variant — [`AgentMessageKind::Wire`] —
//! containing a `Message`. New variants land alongside their use
//! sites; persistence and projection-to-LLM call sites match
//! exhaustively on `AgentMessageKind` so any addition forces a
//! conscious migration.
//!
//! See `docs/aj-next-plan.md` §1.4 and §2.0(b).

use aj_models::types::Message;
use serde::{Deserialize, Serialize};

/// A single entry in the agent's transcript.
///
/// Wire format is transparent: `AgentMessage` serializes exactly as
/// its inner [`AgentMessageKind`], so the on-disk shape is the
/// classifier shape directly (e.g. `{"kind": "wire", ...wire message
/// fields...}`). The struct exists so callers can talk about
/// `AgentMessage` as the public type while the classifier is
/// reserved for the variant tag.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentMessage {
    /// Categorized payload. See [`AgentMessageKind`].
    pub kind: AgentMessageKind,
}

impl AgentMessage {
    /// Wrap a wire [`Message`] as an [`AgentMessage`].
    pub fn wire(message: Message) -> Self {
        Self {
            kind: AgentMessageKind::Wire(message),
        }
    }

    /// Borrow the inner wire [`Message`] if this entry carries one.
    ///
    /// All current entries do; the option exists so future agent-only
    /// variants compose cleanly.
    pub fn as_wire(&self) -> Option<&Message> {
        match &self.kind {
            AgentMessageKind::Wire(m) => Some(m),
        }
    }
}

/// Variants of an [`AgentMessage`].
///
/// `#[serde(tag = "kind", rename_all = "snake_case")]` keeps the
/// on-disk format human-readable and stable across additions.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentMessageKind {
    /// A wire-level message — user, assistant, or tool result.
    /// Projects directly onto the LLM context.
    Wire(Message),
}

impl From<Message> for AgentMessage {
    fn from(message: Message) -> Self {
        Self::wire(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_models::types::{TextContent, UserContent, UserMessage};

    #[test]
    fn agent_message_round_trips_through_json() {
        // Lock the on-disk shape the upcoming `aj-session` log relies
        // on: `{"kind": "wire", ...}` framing with the wire `Message`
        // payload nested inside.
        let msg = AgentMessage::from(Message::User(UserMessage {
            content: vec![UserContent::Text(TextContent {
                text: "hi".to_string(),
                text_signature: None,
            })],
            timestamp: 42,
        }));

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["kind"], "wire");

        let round_tripped: AgentMessage = serde_json::from_value(json).unwrap();
        match round_tripped.kind {
            AgentMessageKind::Wire(Message::User(u)) => {
                assert_eq!(u.timestamp, 42);
                assert_eq!(u.content.len(), 1);
            }
            other => panic!("expected Wire(User), got {other:?}"),
        }
    }
}
