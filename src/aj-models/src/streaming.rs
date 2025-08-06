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
    ProtocolError {
        error: String,
    },
}
