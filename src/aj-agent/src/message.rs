//! Agent-level transcript entries.
//!
//! `AgentMessage` is the unit of the agent's in-memory transcript: the
//! input shape for `Agent::seed_session` and the output shape for
//! `Agent::messages()`. It wraps wire-level [`Message`]s behind an enum
//! so the transcript can also hold agent-only entries (UI-only
//! annotations, tool batches, system-prompt anchors) that are not valid
//! wire messages, without those leaking into the event protocol.
//!
//! [`AgentMessageKind`] is the forward-compat seam: an agent-only entry
//! becomes a new variant. [`AgentMessageKind::TaskNotification`] is one
//! such entry: a background task's completion notice, stored as typed
//! data and framed into a wire user message only when it projects onto
//! the provider (see [`AgentMessage::to_projected_wire`]). Every
//! persistence and projection-to-LLM call site matches exhaustively on
//! the enum, so adding a variant forces each consumer to decide how to
//! handle it.

use aj_models::types::{Message, UserMessage};
use serde::{Deserialize, Serialize};

use crate::tool::{TASK_NOTIFICATION_CLOSE_TAG, TASK_NOTIFICATION_OPEN_TAG};

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
    /// on-disk entry id when resuming a session, and by the remote-control
    /// codec to backfill it from a durable event frame.
    ///
    /// Setting an empty id would break the non-empty `message_id` contract
    /// that the reducer and branch operations depend on.
    pub fn set_id(&mut self, id: String) {
        self.id = id;
    }

    /// Wrap a [`TaskNotification`] as an [`AgentMessage`], minting a
    /// fresh id like [`AgentMessage::wire`].
    pub fn task_notification(notification: TaskNotification) -> Self {
        Self {
            id: format!("{:032x}", rand::random::<u128>()),
            kind: AgentMessageKind::TaskNotification(notification),
        }
    }

    /// Borrow the wire [`Message`] this entry *literally stores*, or
    /// `None` for agent-only kinds that have no stored wire form.
    ///
    /// A [`AgentMessageKind::TaskNotification`] returns `None`: it is
    /// stored as typed data, not as a wire message. Callers that need
    /// the provider-facing projection want [`Self::to_projected_wire`]
    /// instead.
    pub fn as_stored_wire(&self) -> Option<&Message> {
        match &self.kind {
            AgentMessageKind::Wire(m) => Some(m),
            AgentMessageKind::TaskNotification(_) => None,
        }
    }

    /// The wire [`Message`] the provider receives for this entry.
    ///
    /// A stored wire message projects as itself. A task notification
    /// synthesizes a user message with the task-notification framing,
    /// which is the only text the model ever sees for a notice. `None`
    /// only for future kinds that never project onto the wire.
    pub fn to_projected_wire(&self) -> Option<Message> {
        match &self.kind {
            AgentMessageKind::Wire(m) => Some(m.clone()),
            AgentMessageKind::TaskNotification(n) => {
                // Byte-identical to the pre-typed tagged string: the
                // delimiters sit on their own lines so the body renders
                // as regular markdown between them.
                let text = format!(
                    "{TASK_NOTIFICATION_OPEN_TAG}\n{}\n{TASK_NOTIFICATION_CLOSE_TAG}",
                    n.body,
                );
                Some(Message::User(UserMessage::text(text)))
            }
        }
    }
}

/// A background task's completion notice, stored as typed transcript
/// data rather than a tagged user message.
///
/// The structure is kept for rich local rendering. Only [`Self::body`]
/// reaches the model, framed by [`AgentMessage::to_projected_wire`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskNotification {
    /// On-disk discriminator so untagged deserialization picks this
    /// over a wire [`Message`]. Serializes to exactly
    /// `"task_notification"`, a `role` value no wire message uses.
    #[serde(rename = "role")]
    tag: NotificationTag,
    /// Command line (bash) or task description (agent).
    pub label: String,
    /// What kind of work ran, for icon/label selection.
    pub kind: TaskNotificationKind,
    /// Terminal outcome.
    pub outcome: TaskOutcome,
    /// Pre-rendered notice body: exit status + output tail (bash) or
    /// the report (agent). This is the only text projected to the
    /// model.
    pub body: String,
}

impl TaskNotification {
    /// Build a notification from its already-rendered parts. The body
    /// is stored verbatim, so callers trim it to the exact model-facing
    /// text before constructing.
    pub fn new(
        label: String,
        kind: TaskNotificationKind,
        outcome: TaskOutcome,
        body: String,
    ) -> Self {
        Self {
            tag: NotificationTag::TaskNotification,
            label,
            kind,
            outcome,
            body,
        }
    }
}

/// On-disk `role` discriminator for [`TaskNotification`]. A unit enum
/// that accepts only `"task_notification"`, so a `role:"user"` line
/// never deserializes as a notification and vice versa.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum NotificationTag {
    #[serde(rename = "task_notification")]
    TaskNotification,
}

/// What kind of work a completed background task ran, for icon/label
/// selection.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskNotificationKind {
    Bash,
    Agent,
}

/// Terminal outcome of a completed background task.
///
/// Internally tagged (`status`) so a consumer always reads a
/// `status` field, with `code` present only for a failing exit.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TaskOutcome {
    /// Exited 0 (bash) or completed (agent).
    Succeeded,
    /// Exited non-zero, or was signal-killed (`code: None`), or an
    /// agent run failed.
    Failed { code: Option<i32> },
    /// Killed via `task_stop`, the TUI, or shutdown.
    Killed,
}

/// Variants of an [`AgentMessage`].
///
/// `#[serde(untagged)]` keeps the on-disk shape flat: each variant
/// writes its own payload directly with no outer discriminator. Each
/// variant carries its own `role` discriminator inside the payload so
/// `untagged` deserialization can disambiguate:
///
/// - [`AgentMessageKind::Wire`] serializes as a bare wire [`Message`]
///   (`{"role": "user", ...}`), with `role` one of `user` / `assistant`
///   / `tool_result`.
/// - [`AgentMessageKind::TaskNotification`] carries
///   `role:"task_notification"`, a value no wire message uses.
///
/// `Wire` must stay first: `Message` is `#[serde(tag = "role")]`, so a
/// `role:"task_notification"` line fails to parse as `Wire` (unknown
/// role) and falls through to `TaskNotification`. The required `role`
/// discriminator on `TaskNotification` makes it reject a `role:"user"`
/// line, so both directions disambiguate on `role`.
///
/// Backward compatibility: legacy lines written before the untagged
/// format change carry a stray `"kind": "wire"` field. `Message` has no
/// `#[serde(deny_unknown_fields)]`, so the extra field is silently
/// ignored on read.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessageKind {
    /// A wire-level message — user, assistant, or tool result.
    /// Projects directly onto the LLM context.
    Wire(Message),
    /// A background task's completion notice. Stored as typed data,
    /// projected onto the wire as a framed user message (see
    /// [`AgentMessage::to_projected_wire`]).
    TaskNotification(TaskNotification),
}

impl From<Message> for AgentMessage {
    fn from(message: Message) -> Self {
        Self::wire(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_models::types::{TextContent, UserContent};

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

    #[test]
    fn task_notification_round_trips_through_json() {
        let msg = AgentMessage::task_notification(TaskNotification::new(
            "sleep 1".to_string(),
            TaskNotificationKind::Bash,
            TaskOutcome::Failed { code: Some(2) },
            "exit code 2".to_string(),
        ));

        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["role"], "task_notification");
        assert_eq!(json["label"], "sleep 1");
        assert_eq!(json["kind"], "bash");
        assert_eq!(json["outcome"]["status"], "failed");
        assert_eq!(json["outcome"]["code"], 2);

        let round_tripped: AgentMessage = serde_json::from_value(json).unwrap();
        match round_tripped.kind {
            AgentMessageKind::TaskNotification(n) => {
                assert_eq!(n.label, "sleep 1");
                assert_eq!(n.kind, TaskNotificationKind::Bash);
                assert_eq!(n.outcome, TaskOutcome::Failed { code: Some(2) });
                assert_eq!(n.body, "exit code 2");
            }
            other => panic!("expected TaskNotification, got {other:?}"),
        }
    }

    #[test]
    fn user_line_never_parses_as_task_notification() {
        // A `role:"user"` line must land on `Wire(User)`, never on the
        // notification variant, so a real prompt is never mistaken for
        // a harness notice.
        let json = serde_json::json!({
            "role": "user",
            "content": [{"type": "text", "text": "hi"}],
            "timestamp": 0,
        });
        let msg: AgentMessage = serde_json::from_value(json).unwrap();
        assert!(
            matches!(msg.kind, AgentMessageKind::Wire(Message::User(_))),
            "role:user must parse as Wire(User), got {:?}",
            msg.kind
        );
    }

    #[test]
    fn task_notification_line_never_parses_as_wire() {
        // The `task_notification` role is unknown to `Message`, so the
        // line falls through the untagged `Wire` variant onto the
        // typed notification.
        let json = serde_json::json!({
            "role": "task_notification",
            "label": "sleep 1",
            "kind": "agent",
            "outcome": {"status": "succeeded"},
            "body": "done",
        });
        let msg: AgentMessage = serde_json::from_value(json).unwrap();
        match msg.kind {
            AgentMessageKind::TaskNotification(n) => {
                assert_eq!(n.kind, TaskNotificationKind::Agent);
                assert_eq!(n.outcome, TaskOutcome::Succeeded);
            }
            other => panic!("expected TaskNotification, got {other:?}"),
        }
    }

    #[test]
    fn task_notification_projects_to_framed_user_message() {
        let msg = AgentMessage::task_notification(TaskNotification::new(
            "sleep 1".to_string(),
            TaskNotificationKind::Bash,
            TaskOutcome::Succeeded,
            "exit code 0".to_string(),
        ));
        // No stored wire message, but it projects onto a framed user
        // message.
        assert!(msg.as_stored_wire().is_none());
        match msg.to_projected_wire() {
            Some(Message::User(u)) => {
                let text = match &u.content[0] {
                    UserContent::Text(t) => &t.text,
                    other => panic!("expected text, got {other:?}"),
                };
                assert_eq!(
                    text,
                    "<task-notification>\nexit code 0\n</task-notification>"
                );
            }
            other => panic!("expected User projection, got {other:?}"),
        }
    }
}
