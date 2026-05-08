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

use std::time::Duration;

use aj_models::messages::ContentBlockParam;
use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{AssistantMessage, ToolResultMessage};
use aj_ui::{TokenUsage, UserOutput};
use serde_json::Value;

use crate::message::AgentMessage;
use crate::tool::ToolDetails;

/// Identifier for the agent emitting an event.
///
/// `Main` is the top-level agent in a session; `Sub(n)` is the n-th
/// sub-agent spawned through the `agent` tool. Sub-agent indices are
/// assigned by the parent's session state and are stable for the
/// lifetime of the parent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AgentId {
    /// The top-level agent.
    Main,
    /// A sub-agent identified by its assignment index.
    Sub(usize),
}

/// Channel for streaming content. Routed by listeners onto distinct
/// UI components — visible text vs. extended thinking vs. replayed
/// user input — so renderers can style them differently and
/// persistence can decide whether each is worth keeping (today: only
/// the finalized message is persisted; streaming is transient).
///
/// [`StreamChannel::User`] is used exclusively by replay (see
/// [`aj_session::replay`]): live user input arrives via the readline
/// loop and never flows through the bus, but resuming a thread
/// projects each persisted user-role text message as a Start/Stop
/// pair on the user channel so the renderer can repaint the prior
/// turns through the same path it uses for live streaming.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamChannel {
    /// Visible assistant text.
    Text,
    /// Extended thinking ("reasoning") content.
    Thinking,
    /// Replayed user input. Only emitted during conversation replay;
    /// live user prompts are echoed by the terminal directly.
    User,
}

/// Streaming progress action for an in-flight assistant content block.
///
/// `Start`, `Update`, `Stop` mirror the legacy `agent_text_*` /
/// `agent_thinking_*` callback shape so the bridge listener in the
/// `aj` binary can drive the existing `AjCli` rendering helpers
/// without translation. Once the agent migrates to unified streaming
/// (`AssistantMessageEvent`) in §2.4, this enum and the
/// [`AgentEvent::StreamChunk`] variant fold into [`AgentEvent::MessageUpdate`].
#[derive(Clone, Debug)]
pub enum StreamAction {
    /// First fragment of a new block. `snapshot` is the initial chunk
    /// the renderer should display.
    Start { snapshot: String },
    /// Incremental delta. `delta` is the bytes appended since the last
    /// event on this channel.
    Update { delta: String },
    /// Block finalized. `snapshot` is the final, complete content of
    /// the block; renderers that drop intermediate `Update`s can fall
    /// back on this for the full text.
    Stop { snapshot: String },
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
#[derive(Clone, Debug)]
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
    ToolResult { content: Vec<ContentBlockParam> },
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
#[derive(Clone, Debug)]
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
    /// assistant, and tool-result messages alike.
    MessageStart {
        agent_id: AgentId,
        message: AgentMessage,
    },
    /// Streaming update for an assistant message in flight. Carries
    /// the underlying provider [`AssistantMessageEvent`] so listeners
    /// can drive incremental rendering, plus the partial
    /// [`AgentMessage`] snapshot for callers that just want the
    /// running text/thinking/tool-call state.
    MessageUpdate {
        agent_id: AgentId,
        message: AgentMessage,
        event: AssistantMessageEvent,
    },
    /// A message has been finalized.
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

    // --- Streaming bridge (§2.3 → §2.4) -----------------------------------
    /// Streaming progress for an in-flight assistant content block.
    ///
    /// Bridging shape used while the agent still drives inference
    /// through the legacy `Model::run_inference_streaming` /
    /// `StreamingEvent` pipeline. In §2.4 this folds into
    /// [`AgentEvent::MessageUpdate`] carrying an
    /// [`AssistantMessageEvent`]; until then, [`StreamChannel`] +
    /// [`StreamAction`] cover what the legacy CLI renderer needs.
    StreamChunk {
        agent_id: AgentId,
        channel: StreamChannel,
        action: StreamAction,
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
    /// Bridging shape used while the agent still drives inference
    /// through the legacy [`MessageParam`](aj_models::messages::MessageParam)
    /// flow. Folds into [`AgentEvent::MessageEnd`] in §2.4 once
    /// `aj-agent` migrates to the unified
    /// [`aj_models::types::Message`] types and the on-disk format
    /// switches to typed message entries.
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
            | Self::StreamChunk { agent_id, .. }
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
}
