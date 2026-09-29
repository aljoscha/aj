//! Display-oriented data types carried on bus events.
//!
//! [`TokenUsage`] is a structured token-count snapshot the renderer formats.
//! It rides on [`crate::events::AgentEvent::UsageUpdate`] or
//! [`crate::events::AgentEvent::CompactionEnd`] after every accounted
//! assistant turn or committed compaction.

use serde::{Deserialize, Serialize};

/// Per-operation token-usage snapshot suitable for an at-a-glance
/// renderer. Carries both operation-local and accumulated counts so the
/// caller doesn't need to subtract.
///
/// The accumulator semantics match what the agent maintains in
/// [`crate::Agent::accumulated_usage`]: every successful accounted operation
/// adds its [`aj_models::types::Usage`] into the accumulator. The
/// snapshot here is taken *before* that add, so `accumulated_*`
/// reflects the running total **observed before this operation was
/// folded in**. Together with `turn_*`, a single event answers the
/// question "what was there before, and what is this operation adding"
/// — the running total afterwards is exactly
/// `accumulated_* + turn_*`. Field names mirror the unified usage
/// shape (`input`, `output`, `cache_read`, `cache_write`).
///
/// Polling [`crate::Agent::accumulated_usage`] *between* turns
/// returns the post-add total (i.e. the next `UsageUpdate` event's
/// `accumulated_* + turn_*`), so a consumer that needs the
/// "current running total at any instant" can either read the
/// getter or maintain its own sum off the bus events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub accumulated_input: u64,
    pub turn_input: u64,
    pub accumulated_output: u64,
    pub turn_output: u64,
    pub accumulated_cache_write: u64,
    pub turn_cache_write: u64,
    pub accumulated_cache_read: u64,
    pub turn_cache_read: u64,
}
