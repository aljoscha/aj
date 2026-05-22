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

use std::collections::HashMap;
use std::time::Duration;

use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{AssistantMessage, ToolResultMessage};
use aj_models::wire::ContentBlockParam;
use serde::Serialize;
use serde_json::Value;

use crate::message::AgentMessage;
use crate::tool::ToolDetails;
use crate::types::{TokenUsage, UserOutput};

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

/// What the persistence listener should append to the log when it
/// observes an [`AgentEvent::MessagePersisted`].
///
/// Each variant maps directly onto a single
/// `aj_session::ConversationView` write the persistence listener
/// performs in response. Carrying the payload on the event keeps
/// the listener stateless (it doesn't need to know anything about
/// turn structure — it just appends what it's told). Folds into
/// the per-message [`AgentEvent::MessageEnd`] payload in §2.4 when
/// the on-disk format switches to typed messages.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PersistedMessageKind {
    /// Append a user-role message carrying typed user input — the
    /// readline loop's prompt for the top-level agent, or a
    /// sub-agent's spawning task. Maps onto
    /// `aj_session::ConversationView::add_user_message`.
    User { content: Vec<ContentBlockParam> },
    /// Append an assistant message with the given content blocks.
    /// Maps onto `aj_session::ConversationView::add_assistant_message`.
    Assistant { content: Vec<ContentBlockParam> },
    /// Append a user-role message carrying one or more
    /// `tool_result` content blocks. Synthesized by the agent at
    /// the end of every tool batch so the next inference sees the
    /// model's tool calls answered. Maps onto
    /// `aj_session::ConversationView::add_user_message`.
    ///
    /// `details` carries the structured per-call [`ToolDetails`]
    /// payload the agent emitted alongside the wire content, keyed
    /// by `tool_use_id`. Every entry in `content` that is a
    /// `ContentBlockParam::ToolResultBlock` has a matching entry
    /// here; persistence rides both halves on the same on-disk
    /// record so a resumed session can rehydrate the structured
    /// renderer state (diffs, todo snapshots, bash exit codes,
    /// sub-agent reports) instead of falling back to the wire
    /// text-only projection. The map is `skip_serializing_if`
    /// empty so legacy emitters and the rare details-less batch
    /// don't pollute the JSON-on-the-wire shape.
    ToolResult {
        content: Vec<ContentBlockParam>,
        #[serde(default, skip_serializing_if = "HashMap::is_empty")]
        details: HashMap<String, ToolDetails>,
    },
    /// Append a freestanding [`UserOutput`] entry. Today only
    /// emitted for the synthesized [`UserOutput::ToolError`]
    /// records the agent writes when a tool-call's `input` JSON
    /// fails to parse or the tool's `execute` itself returns an
    /// `Err`. Folds into the structured [`ToolDetails`] entry shape
    /// in §2.4 (see `docs/aj-next-plan.md` §3 — log-format
    /// migration).
    UserOutput { output: UserOutput },
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
    },
    /// A tool call has finalized. `result` is the structured payload
    /// that flows into the persistence log; `is_error` mirrors the
    /// flag projected onto the wire `tool_result` message.
    ToolExecutionEnd {
        agent_id: AgentId,
        call_id: String,
        tool: String,
        result: ToolDetails,
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
    /// [`AgentEvent::MessageEnd`] payloads. Folds into the
    /// `AssistantMessage.usage` ride-along on
    /// [`AgentEvent::MessageEnd`] in §2.4.
    TurnUsage {
        agent_id: AgentId,
        usage: TokenUsage,
    },

    /// Persistence write request for a finalized turn entry.
    ///
    /// Emitted by the agent every time an in-flight turn produces a
    /// payload that must hit disk: the assistant message itself, the
    /// synthesized user message carrying tool-result content blocks,
    /// or one of the freestanding [`UserOutput`] entries written for
    /// tool-call parse failures and tool-execution errors. The
    /// persistence listener responds by appending one
    /// [`aj_session::ConversationEntry`] to the log; the bus's
    /// awaited-inline listener semantics give us the same "log is
    /// never more than one event behind reality" durability
    /// guarantee the previous direct `view.add_*_message` calls had,
    /// and a listener that returns `Err` (e.g. on a disk-write
    /// failure) aborts the run via [`crate::TurnError::Fatal`].
    ///
    /// Bridging shape: the agent's in-memory transcript still uses
    /// the wire-shaped [`MessageParam`](aj_models::wire::MessageParam)
    /// rather than the tagged-union
    /// [`aj_models::types::Message`], so this event carries the wire
    /// shape directly. A future migration may swap both the
    /// transcript and the on-disk format onto the typed shape; this
    /// variant folds into [`AgentEvent::MessageEnd`] then.
    MessagePersisted {
        agent_id: AgentId,
        kind: PersistedMessageKind,
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
            | Self::MessagePersisted { agent_id, .. }
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
    /// enums (`AgentId`, `AssistantMessageEvent`,
    /// `PersistedMessageKind`). Adding a new event or renaming a field
    /// shows up here as a test breakage instead of silently changing
    /// the consumer-facing shape.
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

        let persisted = AgentEvent::MessagePersisted {
            agent_id: AgentId::Main,
            kind: PersistedMessageKind::User { content: vec![] },
        };
        let json = serde_json::to_value(&persisted).expect("persisted serializes");
        assert_eq!(json["type"], "message_persisted");
        assert_eq!(json["kind"]["kind"], "user");
        assert!(json["kind"]["content"].is_array());

        // The `ToolResult` variant carries an additional `details`
        // map keyed by `tool_use_id`. Empty maps are skipped so the
        // common case (an empty / legacy details payload) stays
        // identical to the pre-widening wire shape; a populated map
        // surfaces as a JSON object whose values are the
        // internally-tagged `ToolDetails` shape.
        let persisted_empty = AgentEvent::MessagePersisted {
            agent_id: AgentId::Main,
            kind: PersistedMessageKind::ToolResult {
                content: vec![],
                details: HashMap::new(),
            },
        };
        let json = serde_json::to_value(&persisted_empty).expect("empty tool-result serializes");
        assert_eq!(json["kind"]["kind"], "tool_result");
        assert!(json["kind"]["content"].is_array());
        assert!(
            json["kind"].get("details").is_none(),
            "empty details map should be skipped on serialize: {json}"
        );

        let mut details = HashMap::new();
        details.insert(
            "tu-1".to_string(),
            ToolDetails::Text {
                summary: "ping".to_string(),
                body: "pong".to_string(),
            },
        );
        let persisted_with_details = AgentEvent::MessagePersisted {
            agent_id: AgentId::Main,
            kind: PersistedMessageKind::ToolResult {
                content: vec![],
                details,
            },
        };
        let json = serde_json::to_value(&persisted_with_details)
            .expect("tool-result with details serializes");
        assert_eq!(json["kind"]["details"]["tu-1"]["kind"], "text");
        assert_eq!(json["kind"]["details"]["tu-1"]["summary"], "ping");
        assert_eq!(json["kind"]["details"]["tu-1"]["body"], "pong");
    }
}
