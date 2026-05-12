//! OpenAI provider integration.
//!
//! Hosts the unified [`Provider`](crate::provider::Provider)
//! implementations for the three OpenAI APIs: Chat Completions in
//! [`provider`] (per `docs/models-spec.md` §7.2), Responses in
//! [`responses`] (per §7.3), and the Codex Responses variant in
//! [`codex`] (per §7.4).

pub mod codex;
pub mod provider;
pub mod responses;

pub use codex::OpenAiCodexResponsesProvider;
pub use provider::OpenAiCompletionsProvider;
pub use responses::OpenAiResponsesProvider;
