//! OpenAI provider integration.
//!
//! Hosts the unified [`Provider`](crate::provider::Provider)
//! implementations for the three OpenAI APIs: Chat Completions in
//! [`provider`], Responses in
//! [`responses`], and the Codex Responses variant in
//! [`codex`].

pub mod codex;
pub mod errors;
pub mod provider;
pub mod responses;

pub use codex::OpenAiCodexResponsesProvider;
pub use provider::OpenAiCompletionsProvider;
pub use responses::OpenAiResponsesProvider;
