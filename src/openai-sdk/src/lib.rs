//! A minimal SDK for the OpenAI API. Covers the Chat Completions,
//! Responses, and Codex Responses endpoints, with both blocking and
//! streaming variants.
//!
//! Only the surface AJ needs is modeled. Unknown wire shapes are
//! preserved via `Other(Value)` catch-alls on the streaming-event and
//! output-item enums so schema evolution doesn't break the parser.

pub mod client;
pub mod types;
