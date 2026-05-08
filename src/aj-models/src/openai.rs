//! OpenAI provider integration.
//!
//! Hosts the new unified [`Provider`](crate::provider::Provider)
//! implementation for OpenAI's Chat Completions API in [`provider`]
//! (per `docs/models-spec.md` §7.2) alongside the legacy
//! [`Model`](crate::Model)-based Responses-API client in [`legacy`],
//! kept around until the agent migration in §12.16 lands.

pub mod legacy;
pub mod provider;
pub mod responses;

// Re-exported so existing call sites that reference
// `crate::openai::OpenAiModel` keep working without churn.
pub use legacy::OpenAiModel;
pub use provider::OpenAiCompletionsProvider;
pub use responses::OpenAiResponsesProvider;
