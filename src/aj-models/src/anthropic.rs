//! Anthropic provider integration.
//!
//! Hosts the unified [`Provider`](crate::provider::Provider)
//! implementation in [`provider`] per `docs/models-spec.md` §6.

pub mod provider;

pub use provider::AnthropicProvider;
