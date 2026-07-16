//! Agent-level transcript entries.
//!
//! `AgentMessage` is the unit of the agent's in-memory transcript: the
//! input shape for `Agent::seed_session` and the output shape for
//! `Agent::messages()`. It wraps wire-level [`Message`]s behind an enum
//! so the transcript can also hold agent-only entries (UI-only
//! annotations, tool batches, system-prompt anchors) that are not valid
//! wire messages, without those leaking into the event protocol.
//!
//! The enum has a single variant, [`AgentMessageKind::Wire`]. That is
//! the forward-compat seam: an agent-only entry becomes a new
//! `AgentMessageKind` variant. Every persistence and projection-to-LLM
//! call site matches exhaustively on the enum, so adding one forces
//! each consumer to decide how to handle it.

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
/// stays implicit in the payload shape. Each new variant must be
/// distinguishable from `Message` by its own discriminator field
/// (typically a distinct `role` value), so `untagged` deserialization
/// can pick the right variant without an outer tag.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentMessage {
    /// In-memory unique id, minted at construction. Never serialized:
    /// `#[serde(skip)]` keeps the wire-JSON shape (a bare [`Message`])
    /// intact, which is a locked on-disk contract, and `#[serde(transparent)]`
    /// still sees exactly one serialized field (`kind`). A deserialized
    /// `AgentMessage` therefore has an empty id until a consumer backfills
    /// it. The log adopts this id as the entry id for message entries and
    /// backfills it from the entry id on resume.
    ///
    /// NOTE: `MessageStart` and `MessageEnd` of one assistant turn are
    /// separate `wire()` constructions, so they carry different ids. Only
    /// `MessageEnd` ids are consumed.
    #[serde(skip)]
    id: String,
    /// Categorized payload. See [`AgentMessageKind`].
    pub kind: AgentMessageKind,
}

impl AgentMessage {
    /// Wrap a wire [`Message`] as an [`AgentMessage`], minting a fresh id.
    ///
    /// This is the single choke point where message ids are minted: every
    /// other constructor routes through here. The id is a 128-bit random
    /// hex token, wide enough that collisions are negligible without a
    /// central registry to check against.
    pub fn wire(message: Message) -> Self {
        Self {
            id: format!("{:032x}", rand::random::<u128>()),
            kind: AgentMessageKind::Wire(message),
        }
    }

    /// The message's unique id. Empty for a message deserialized outside
    /// the log's backfill path (see [`AgentMessage::set_id`]).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Set the message's id. Used by the log to backfill the id from the
    /// on-disk entry id when resuming a session.
    pub fn set_id(&mut self, id: String) {
        self.id = id;
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
/// writes its own payload directly with no outer discriminator. The
/// only variant is [`AgentMessageKind::Wire`], which serializes as a
/// bare wire [`Message`] (`{"role": "user", ...}`). A second variant
/// must carry its own discriminator inside the payload so `untagged`
/// deserialization can disambiguate, typically a `role` value distinct
/// from `user` / `assistant` / `tool_result`.
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

    #[test]
    fn wire_mints_unique_nonempty_ids() {
        let make = || {
            AgentMessage::from(Message::User(UserMessage {
                content: vec![UserContent::Text(TextContent {
                    text: "hi".to_string(),
                    text_signature: None,
                })],
                timestamp: 0,
            }))
        };
        let a = make();
        let b = make();
        assert_eq!(a.id().len(), 32, "id is a 32-hex token");
        assert!(!a.id().is_empty());
        assert_ne!(a.id(), b.id(), "each construction mints a fresh id");
    }

    #[test]
    fn deserialized_message_has_empty_id_until_set() {
        let json = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "hi"}],
            "timestamp": 0,
        });
        let mut msg: AgentMessage = serde_json::from_value(json).unwrap();
        assert!(msg.id().is_empty(), "id is skipped on the wire, so empty");
        msg.set_id("deadbeef".to_string());
        assert_eq!(msg.id(), "deadbeef");
    }
}
