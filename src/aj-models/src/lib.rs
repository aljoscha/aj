//! `aj-models` — the wire layer for AJ.
//!
//! This crate hosts the unified message and streaming types defined in
//! `docs/models-spec.md`, the [`Provider`](provider::Provider) trait
//! that concrete API integrations implement, the
//! [`ModelRegistry`](registry::ModelRegistry) that ships the catalog
//! of available models, and the wire-shaped types in [`wire`] used by
//! `aj-session` for on-disk persistence and by `aj-agent` for the
//! in-memory transcript.
//!
//! Everything above the wire (event bus, tools, persistence
//! framing, UI) lives in `aj-agent`, `aj-session`, and the binary.

pub mod anthropic;
pub mod auth;
pub mod errors;
pub mod oauth;
pub mod openai;
pub mod partial_json;
pub mod provider;
pub mod refresh;
pub mod registry;
pub mod scripted;
pub mod streaming;
pub mod tools;
pub mod transform;
pub mod types;
pub mod wire;

/// Thinking-policy enum used by the agent and the binary's UI to
/// describe the user's preferred reasoning depth.
///
/// The agent projects this onto the unified
/// [`crate::types::ThinkingLevel`] before each inference: `Low`,
/// `Medium`, `High` map directly, while `XHigh` and `Max` both
/// collapse onto [`crate::types::ThinkingLevel::XHigh`] (the unified
/// ceiling). `None` (i.e. `Option<ThinkingConfig>::None`) means
/// "extended thinking off" — different from
/// [`crate::types::ThinkingLevel::Minimal`], which is the lowest
/// effort rung for reasoning models that don't support disabling
/// thinking entirely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThinkingConfig {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}
