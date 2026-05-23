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
/// On-disk shape is the inner [`AgentMessageKind`] flattened: the
/// transparent struct wrapper plus `#[serde(untagged)]` on the enum
/// mean a `Wire(Message)` value serializes as bare wire-message JSON
/// (`{"role": "user", ...}`), with no extra `kind` discriminator on
/// the line. This matches the shape an LLM SDK expects when the
/// `message` field of a session entry is read back.
///
/// The wrapper exists as a forward-compat seam: when we add agent-only
/// transcript content that isn't a wire message (e.g. compaction
/// summaries, sub-agent result summaries, UI-only annotations), they
/// land as additional [`AgentMessageKind`] variants. Disambiguation
/// stays implicit in the payload shape — each new variant must be
/// distinguishable from `Message` by its own discriminator field
/// (typically a distinct `role` value), so `untagged` deserialization
/// can pick the right variant without an outer tag.
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
/// `#[serde(untagged)]` keeps the on-disk shape flat: each variant
/// writes its own payload directly with no outer discriminator. Today
/// the only variant is [`AgentMessageKind::Wire`], which serializes as
/// a bare wire [`Message`] (`{"role": "user", ...}`). When a second
/// variant is added it must carry its own discriminator inside the
/// payload so `untagged` deserialization can disambiguate — typically
/// a `role` value distinct from `user` / `assistant` / `tool_result`.
///
/// Backward compatibility: legacy lines written before this format
/// change carry a stray `"kind": "wire"` field. `Message` has no
/// `#[serde(deny_unknown_fields)]`, so the extra field is silently
/// ignored on read.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
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
        // Lock the on-disk shape `aj-session` writes: a bare wire
        // [`Message`] payload nested under the entry's `message` key,
        // with no outer `kind` discriminator. The transparent struct
        // wrapper + `#[serde(untagged)]` on the kind enum together
        // flatten the wire variant down to its inner JSON.
        let msg = AgentMessage::from(Message::User(UserMessage {
            content: vec![UserContent::Text(TextContent {
                text: "hi".to_string(),
                text_signature: None,
            })],
            timestamp: 42,
        }));

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "user");
        assert!(
            json.get("kind").is_none(),
            "wire variant must not emit a 'kind' tag: {json}"
        );

        let round_tripped: AgentMessage = serde_json::from_value(json).unwrap();
        match round_tripped.kind {
            AgentMessageKind::Wire(Message::User(u)) => {
                assert_eq!(u.timestamp, 42);
                assert_eq!(u.content.len(), 1);
            }
            other => panic!("expected Wire(User), got {other:?}"),
        }
    }

    #[test]
    fn agent_message_deserialize_tolerates_legacy_kind_tag() {
        // Lines written before the flattening change carry a stray
        // `"kind": "wire"` field. `Message` doesn't deny unknown
        // fields, so untagged deserialization sees the `role` tag,
        // picks `Wire(Message::User)`, and silently drops the extra
        // `kind` field. Persistence reads of pre-change thread files
        // depend on this.
        let legacy = serde_json::json!({
            "kind": "wire",
            "role": "user",
            "content": [{"type": "text", "text": "hi"}],
            "timestamp": 42,
        });

        let msg: AgentMessage = serde_json::from_value(legacy).expect("legacy line parses");
        match msg.kind {
            AgentMessageKind::Wire(Message::User(u)) => {
                assert_eq!(u.timestamp, 42);
                assert_eq!(u.content.len(), 1);
            }
            other => panic!("expected Wire(User), got {other:?}"),
        }
    }
}
