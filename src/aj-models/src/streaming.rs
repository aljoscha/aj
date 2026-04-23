//! Types for a higher-level streaming API built on the low-level messages API.
//! Most model APIs/SDKs have a server-sent event API that must then be
//! translated to this one.

use crate::messages::{ApiError, Citation, Message, UsageDelta};

// High-level streaming events with both diff updates and a running snapshot of
// the accumulated message.
#[derive(Clone, Debug)]
pub enum StreamingEvent {
    MessageStart {
        message: Message,
    },
    UsageUpdate {
        usage: UsageDelta,
    },
    FinalizedMessage {
        message: Message,
    },
    Error {
        error: ApiError,
    },
    TextStart {
        text: String,
        citations: Vec<Citation>,
    },
    TextUpdate {
        diff: String,
        snapshot: String,
    },
    TextStop {
        /// The complete text block.
        text: String,
    },
    ThinkingStart {
        thinking: String,
    },
    ThinkingUpdate {
        diff: String,
        snapshot: String,
    },
    ThinkingStop,
    ParseError {
        error: String,
        raw_data: String,
    },
    /// A `tool_use` block whose `input` JSON failed to parse. Carries
    /// the id/name so the agent can synthesize a matching `tool_result`.
    ///
    /// NOTE: providers can only emit this once the block has fully
    /// streamed. With incremental tool-call parsing the id would be
    /// known earlier and we could include the partial `input` too.
    ToolUseParseError {
        id: String,
        name: String,
        error: String,
        raw_data: String,
    },
    ProtocolError {
        error: String,
    },
}
