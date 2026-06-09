//! Agent event protocol.
//!
//! The agent emits a typed event for every state transition: lifecycle
//! markers (turn start/end, agent start/end), message lifecycle (start,
//! streaming update, end), tool execution lifecycle (start, update,
//! end), sub-agent correlation, queue snapshots, and transient
//! notices. Listeners subscribed via the agent's bus see every event
//! in registration order; the same event stream powers the UI, the
//! persistence layer, RPC bridges, and tests.
//!
//! Every variant carries an [`AgentId`] so listeners can route
//! sub-agent activity into nested transcripts.
//!
//! See `docs/aj-next-plan.md` §1.1.

use std::sync::Arc;
use std::time::Duration;

use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{AssistantMessage, ToolResultMessage, UserContent};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::message::AgentMessage;
use crate::tool::ToolDetails;
use crate::types::TokenUsage;

/// Serialize an `Arc<[UserContent]>` as a JSON sequence so the
/// event's wire shape matches a plain `Vec<UserContent>` for
/// listeners that ship events out-of-process (`aj --format json`).
fn serialize_content_arc<S>(content: &Arc<[UserContent]>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeSeq;
    let mut seq = serializer.serialize_seq(Some(content.len()))?;
    for block in content.iter() {
        seq.serialize_element(block)?;
    }
    seq.end()
}

/// Snapshot of an agent's bundle identity: which model it talks to
/// and at what thinking effort and inference speed.
///
/// `thinking` uses the "off" / "low" / "medium" / "high" / "xhigh" /
/// "max" vocabulary; `speed` is "standard" or "fast". Carried on
/// [`AgentEvent::SubAgentStart`] and persisted verbatim in the
/// conversation log's sub-agent spawn entries, so the strings are
/// part of the on-disk contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentSettings {
    /// Provider of the model bundle (e.g. "anthropic").
    pub provider: String,
    /// Catalog id of the model.
    pub model_id: String,
    /// Thinking effort: one of "off", "low", "medium", "high",
    /// "xhigh", "max".
    pub thinking: String,
    /// Inference speed: "standard" or "fast".
    pub speed: String,
}

/// Identifier for the agent emitting an event.
///
/// `Main` is the top-level agent in a session; `Sub(n)` is the n-th
/// sub-agent spawned through the `agent` tool. Sub-agent indices are
/// assigned by the parent's session state and are stable for the
/// lifetime of the parent.
///
/// JSON shape (used by `aj --format json` and any other listener
/// that serializes events): `"main"` for [`AgentId::Main`] and
/// `{"sub": N}` for [`AgentId::Sub`]. Variant names are lowercased to
/// match the rest of the event protocol's serde convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentId {
    /// The top-level agent.
    Main,
    /// A sub-agent identified by its assignment index.
    Sub(usize),
}

/// Bus event emitted by the agent runtime.
///
/// Every variant carries an `agent_id` so a single listener (e.g. the
/// persistence writer or the TUI's event pump) can serve both the
/// main agent and any nested sub-agents.
///
/// Serialization shape (used by `aj --format json` and any other
/// listener that ships events out-of-process): an internally tagged
/// JSON object with the variant discriminator under `"type"` and the
/// payload fields lifted to the top level. Every variant that the
/// agent actually emits is fully serializable; round-tripping
/// through JSONL is part of the print-mode contract.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    // --- Lifecycle ---------------------------------------------------------
    /// Emitted once when [`Agent::prompt`](crate::Agent) starts a run.
    AgentStart { agent_id: AgentId },
    /// Emitted once when a run completes. Carries the full transcript
    /// for listeners that want a final snapshot without replaying.
    AgentEnd {
        agent_id: AgentId,
        messages: Vec<AgentMessage>,
    },
    /// Beginning of an assistant-message turn (one inference + any
    /// tool calls it triggers).
    TurnStart { agent_id: AgentId },
    /// End of an assistant-message turn, after tools have finalized.
    TurnEnd {
        agent_id: AgentId,
        message: AssistantMessage,
        tool_results: Vec<ToolResultMessage>,
    },

    // --- Message lifecycle -------------------------------------------------
    /// A message has been added to the transcript. Fired for user,
    /// assistant, and tool-result messages alike. The assistant case
    /// fires once at the top of the streaming inference (with an
    /// empty content vector); the user / tool-result cases fire
    /// when the message is appended to the transcript.
    ///
    /// This event is also the persistence trigger: the persistence
    /// listener subscribed on the bus writes one
    /// [`aj_session::ConversationEntry`] per finalized message off
    /// [`AgentEvent::MessageEnd`].
    MessageStart {
        agent_id: AgentId,
        message: AgentMessage,
    },
    /// Streaming update for an assistant message in flight. Carries
    /// the underlying provider [`AssistantMessageEvent`] (with its
    /// own `partial: AssistantMessage` snapshot) so listeners can
    /// drive incremental rendering. Only fires between
    /// [`MessageStart`] and [`MessageEnd`] for assistant messages.
    MessageUpdate {
        agent_id: AgentId,
        message: AgentMessage,
        event: AssistantMessageEvent,
    },
    /// A message has been finalized. The `message` field is the
    /// authoritative final form; renderers / persistence listeners
    /// should treat any in-flight state from `MessageUpdate` as
    /// stale once this fires.
    ///
    /// Persistence subscribes to this event directly: every
    /// [`AgentEvent::MessageEnd`] becomes one
    /// [`aj_session::ConversationEntry`] appended to the log. The
    /// agent emits one [`AgentEvent::MessageEnd`] per finalized
    /// message — assistant turns, the user prompt at the top of
    /// every run, and one event per tool-result message in a tool
    /// batch.
    MessageEnd {
        agent_id: AgentId,
        message: AgentMessage,
    },

    // --- Tool execution lifecycle -----------------------------------------
    /// A tool call is about to execute. `args` carries the validated
    /// input that will be passed to
    /// [`crate::tool::ToolDefinition::execute`].
    ToolExecutionStart {
        agent_id: AgentId,
        call_id: String,
        tool: String,
        args: Value,
    },
    /// Optional progress update from a long-running tool. `partial`
    /// is a cumulative snapshot, not a delta, so renderers can drop
    /// stale frames without losing state.
    ToolExecutionUpdate {
        agent_id: AgentId,
        call_id: String,
        tool: String,
        args: Value,
        partial: ToolDetails,
        /// Cumulative partial wire content blocks. Empty for tools
        /// that only stream [`ToolDetails`] snapshots without
        /// surfacing partial wire content. `Arc<[…]>` keeps the
        /// event cheap to clone across the bus when an image block
        /// (which can be multiple megabytes of base64) is attached.
        #[serde(serialize_with = "serialize_content_arc")]
        content: Arc<[UserContent]>,
    },
    /// A tool call has finalized. `result` is the structured payload
    /// that flows into the persistence log; `is_error` mirrors the
    /// flag projected onto the wire `tool_result` message.
    ToolExecutionEnd {
        agent_id: AgentId,
        call_id: String,
        tool: String,
        result: ToolDetails,
        /// The finalized tool-result wire content blocks. Mirrors
        /// the `content` field on the corresponding
        /// [`ToolResultMessage`] in the transcript; carried on the
        /// event so live renderers (e.g. inline image display) can
        /// reach the raw block payloads without rummaging through
        /// the transcript. `Arc<[…]>` keeps cloning cheap for
        /// image-bearing results.
        #[serde(serialize_with = "serialize_content_arc")]
        content: Arc<[UserContent]>,
        is_error: bool,
    },

    // --- Sub-agents --------------------------------------------------------
    /// A sub-agent has been spawned. `parent` is the agent that
    /// invoked the `agent` tool; `child` is the freshly assigned
    /// sub-agent id.
    SubAgentStart {
        parent: AgentId,
        child: AgentId,
        task: String,
        /// The child's bundle identity at spawn. Today sub-agents
        /// mirror the parent's bundle. Flattened so the JSON wire
        /// shape keeps the four settings fields at the top level of
        /// the event object.
        #[serde(flatten)]
        settings: AgentSettings,
    },
    /// A sub-agent has finished and returned its report.
    SubAgentEnd {
        parent: AgentId,
        child: AgentId,
        report: String,
    },

    // --- Notices -----------------------------------------------------------
    /// Informational notice. Transient — not persisted.
    Notice { agent_id: AgentId, text: String },
    /// Warning notice. Transient — not persisted.
    Warning { agent_id: AgentId, text: String },
    /// Error notice. Transient — not persisted; the persisted error
    /// state lives on the relevant `AssistantMessage.error`.
    Error { agent_id: AgentId, text: String },
    /// A transient retry of a streaming inference. Surfaces backoff
    /// state so the UI can render a "retrying…" indicator.
    StreamRetry {
        agent_id: AgentId,
        attempt: u32,
        delay: Duration,
        error: String,
    },

    /// Per-turn token usage snapshot. Bridging variant emitted at the
    /// end of each assistant turn so renderers can show a one-line
    /// usage indicator without having to re-aggregate from
    /// [`AgentEvent::MessageEnd`] payloads.
    TurnUsage {
        agent_id: AgentId,
        usage: TokenUsage,
    },

    // --- Queue snapshots ---------------------------------------------------
    /// Full snapshot of both pending-message queues. Fired whenever
    /// either queue changes so listeners can render a single state
    /// from a single event.
    QueueUpdate {
        agent_id: AgentId,
        steering: Vec<AgentMessage>,
        follow_up: Vec<AgentMessage>,
    },
}

impl AgentEvent {
    /// Borrow the [`AgentId`] this event was emitted from.
    ///
    /// For sub-agent correlation events ([`AgentEvent::SubAgentStart`],
    /// [`AgentEvent::SubAgentEnd`]) this returns the parent's id —
    /// the child's id is on the dedicated `child` field.
    pub fn agent_id(&self) -> AgentId {
        match self {
            Self::AgentStart { agent_id }
            | Self::AgentEnd { agent_id, .. }
            | Self::TurnStart { agent_id }
            | Self::TurnEnd { agent_id, .. }
            | Self::MessageStart { agent_id, .. }
            | Self::MessageUpdate { agent_id, .. }
            | Self::MessageEnd { agent_id, .. }
            | Self::ToolExecutionStart { agent_id, .. }
            | Self::ToolExecutionUpdate { agent_id, .. }
            | Self::ToolExecutionEnd { agent_id, .. }
            | Self::Notice { agent_id, .. }
            | Self::Warning { agent_id, .. }
            | Self::Error { agent_id, .. }
            | Self::StreamRetry { agent_id, .. }
            | Self::TurnUsage { agent_id, .. }
            | Self::QueueUpdate { agent_id, .. } => *agent_id,
            Self::SubAgentStart { parent, .. } | Self::SubAgentEnd { parent, .. } => *parent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_id_dispatches_to_parent_for_sub_events() {
        // Every regular event reports its own agent_id; sub-agent
        // correlation events report the parent so listeners that
        // group by source see the spawn alongside its own activity.
        let main_evt = AgentEvent::TurnStart {
            agent_id: AgentId::Main,
        };
        assert_eq!(main_evt.agent_id(), AgentId::Main);

        let sub_spawn = AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(0),
            task: "test".into(),
            settings: AgentSettings {
                provider: "scripted".into(),
                model_id: "scripted-model".into(),
                thinking: "off".into(),
                speed: "standard".into(),
            },
        };
        assert_eq!(sub_spawn.agent_id(), AgentId::Main);

        let sub_evt = AgentEvent::Notice {
            agent_id: AgentId::Sub(3),
            text: "hello".into(),
        };
        assert_eq!(sub_evt.agent_id(), AgentId::Sub(3));
    }

    /// Pin the JSON-on-the-wire shape used by `aj --format json`.
    ///
    /// Listeners that ship events out-of-process (today the print-mode
    /// JSONL writer; tomorrow any RPC bridge) rely on this contract,
    /// so we lock the discriminator key (`"type"`), the `snake_case`
    /// renaming of variant names, and the in-tagged shape of nested
    /// enums (`AgentId`, `AssistantMessageEvent`). Adding a new event
    /// or renaming a field shows up here as a test breakage instead
    /// of silently changing the consumer-facing shape.
    #[test]
    fn agent_event_serializes_with_internally_tagged_snake_case_shape() {
        let evt = AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: "hi".into(),
        };
        let json = serde_json::to_value(&evt).expect("notice serializes");
        assert_eq!(json["type"], "notice");
        assert_eq!(json["agent_id"], "main");
        assert_eq!(json["text"], "hi");

        let sub = AgentEvent::Notice {
            agent_id: AgentId::Sub(7),
            text: "from child".into(),
        };
        let json = serde_json::to_value(&sub).expect("sub-tagged notice serializes");
        // Tuple variants of `AgentId` serialize as `{"sub": N}` so a
        // listener can pattern-match on the discriminator key cheaply.
        assert_eq!(json["agent_id"]["sub"], 7);

        // Round-trip an assistant MessageUpdate carrying a TextDelta
        // event so the in-tagged AssistantMessageEvent shape lands
        // verbatim on the wire — the streaming protocol's discriminator
        // ("type": "text_delta") and the per-block index both reach
        // out-of-process consumers without any custom serializer.
        use aj_models::streaming::AssistantMessageEvent;
        use aj_models::types::AssistantMessage;
        let partial = AssistantMessage {
            content: vec![],
            api: "scripted".into(),
            provider: "scripted".into(),
            model: "scripted".into(),
            response_id: None,
            usage: Default::default(),
            stop_reason: aj_models::types::StopReason::Stop,
            error: None,
            timestamp: 0,
        };
        let update = AgentEvent::MessageUpdate {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(aj_models::types::Message::Assistant(partial.clone())),
            event: AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "abc".into(),
                partial,
            },
        };
        let json = serde_json::to_value(&update).expect("MessageUpdate serializes");
        assert_eq!(json["type"], "message_update");
        assert_eq!(json["event"]["type"], "text_delta");
        assert_eq!(json["event"]["delta"], "abc");
        assert_eq!(json["event"]["content_index"], 0);

        // The flattened `AgentSettings` keeps the four settings
        // fields at the top level of the SubAgentStart object, not
        // nested under a `settings` key.
        let spawn = AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(2),
            task: "explore".into(),
            settings: AgentSettings {
                provider: "anthropic".into(),
                model_id: "claude-x".into(),
                thinking: "medium".into(),
                speed: "fast".into(),
            },
        };
        let json = serde_json::to_value(&spawn).expect("SubAgentStart serializes");
        assert_eq!(json["type"], "sub_agent_start");
        assert_eq!(json["task"], "explore");
        assert_eq!(json["provider"], "anthropic");
        assert_eq!(json["model_id"], "claude-x");
        assert_eq!(json["thinking"], "medium");
        assert_eq!(json["speed"], "fast");
        assert!(json.get("settings").is_none());
    }
}
