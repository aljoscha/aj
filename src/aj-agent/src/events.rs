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

use aj_models::streaming::AssistantMessageEvent;
use aj_models::types::{AssistantMessage, ToolResultMessage};
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
