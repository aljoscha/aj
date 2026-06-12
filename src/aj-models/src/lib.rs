//! `aj-models` — the wire layer for AJ.
//!
//! This crate hosts the unified message and streaming types defined in
//! `docs/models-spec.md`, the [`Provider`](provider::Provider) trait
//! that concrete API integrations implement, the
//! [`ModelRegistry`](registry::ModelRegistry) that ships the catalog
//! of available models, and the runtime types in [`types`] used by
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
pub mod usage;

/// Thinking-policy enum used by the agent and the binary's UI to
/// describe the user's preferred reasoning depth.
///
/// The agent projects this onto the unified
/// [`crate::types::ThinkingLevel`] one-to-one before each inference;
/// each level is sent to the provider verbatim with no remapping.
/// `None` (i.e. `Option<ThinkingConfig>::None`) means "extended
/// thinking off" — different from
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

/// Render an optional [`ThinkingConfig`] as its canonical name:
/// `"off"` for `None`, otherwise one of `"low"`, `"medium"`,
/// `"high"`, `"xhigh"`, `"max"`. This vocabulary is shared by the
/// session log's settings entries, the event protocol, and the
/// binary's level selector.
pub fn thinking_config_name(level: Option<&ThinkingConfig>) -> &'static str {
    match level {
        None => "off",
        Some(ThinkingConfig::Low) => "low",
        Some(ThinkingConfig::Medium) => "medium",
        Some(ThinkingConfig::High) => "high",
        Some(ThinkingConfig::XHigh) => "xhigh",
        Some(ThinkingConfig::Max) => "max",
    }
}

/// Parse a canonical level name back into an optional
/// [`ThinkingConfig`] — the exact inverse of [`thinking_config_name`].
/// Returns `None` for names outside the vocabulary so callers can
/// keep their current level and surface a notice.
pub fn thinking_config_from_name(name: &str) -> Option<Option<ThinkingConfig>> {
    match name {
        "off" => Some(None),
        "low" => Some(Some(ThinkingConfig::Low)),
        "medium" => Some(Some(ThinkingConfig::Medium)),
        "high" => Some(Some(ThinkingConfig::High)),
        "xhigh" => Some(Some(ThinkingConfig::XHigh)),
        "max" => Some(Some(ThinkingConfig::Max)),
        _ => None,
    }
}

/// Render an optional [`types::Speed`] as its canonical name:
/// `"standard"` (also for `None`, the default) or `"fast"`. This
/// vocabulary is shared by the session log's settings entries and
/// the event protocol.
pub fn speed_name(speed: Option<types::Speed>) -> &'static str {
    match speed {
        None | Some(types::Speed::Standard) => "standard",
        Some(types::Speed::Fast) => "fast",
    }
}

/// Parse a canonical speed name back into an optional
/// [`types::Speed`] — the inverse of [`speed_name`], with
/// `"standard"` mapping to `None` (the wire-equivalent default).
/// Returns `None` for names outside the vocabulary so callers can
/// keep their current speed.
pub fn speed_from_name(name: &str) -> Option<Option<types::Speed>> {
    match name {
        "standard" => Some(None),
        "fast" => Some(Some(types::Speed::Fast)),
        _ => None,
    }
}
