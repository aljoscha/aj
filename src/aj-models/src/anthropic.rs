//! Anthropic provider integration.
//!
//! Hosts both the new unified [`Provider`](crate::provider::Provider)
//! implementation in [`provider`] (per `docs/models-spec.md` §6) and the
//! legacy [`Model`](crate::Model)-based implementation in [`legacy`],
//! kept around until the agent migration in §12.16 lands.

pub mod legacy;
pub mod provider;

// Re-exported so existing call sites that reference
// `crate::anthropic::AnthropicModel` keep working without churn.
pub use legacy::AnthropicModel;
pub use provider::AnthropicProvider;
