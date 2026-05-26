//! Display-oriented data types carried on bus events.
//!
//! [`TokenUsage`], [`SubAgentUsage`], and [`UsageSummary`] are
//! structured token-count snapshots the renderer formats.
//! [`TokenUsage`] rides on [`crate::events::AgentEvent::TurnUsage`]
//! at the end of every assistant turn; the summary types are
//! synthesized by the binary at end-of-session.

use serde::{Deserialize, Serialize};

/// Per-turn token-usage snapshot suitable for an at-a-glance
/// renderer. Carries both turn-local and accumulated counts so the
/// caller doesn't need to subtract.
///
/// The accumulator semantics match what the agent maintains in
/// [`crate::Agent::accumulated_usage`]: every successful turn adds
/// its [`aj_models::types::Usage`] into the accumulator. The
/// snapshot here is taken *before* that add, so `accumulated_*`
/// reflects the running total **observed before this turn was
/// folded in**. Together with `turn_*`, a single event answers the
/// question "what was there before, and what is this turn adding"
/// — the running total after the turn is exactly
/// `accumulated_* + turn_*`. Field names mirror the unified usage
/// shape (`input`, `output`, `cache_read`, `cache_write`) per
/// `docs/models-spec.md` §1.3.
///
/// Polling [`crate::Agent::accumulated_usage`] *between* turns
/// returns the post-add total (i.e. the next `TurnUsage` event's
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

/// Per-agent token totals used in [`UsageSummary`]. `agent_id`
/// distinguishes main (`None`) from sub-agents (`Some(n)`); the
/// rendering layer formats each row accordingly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubAgentUsage {
    pub agent_id: Option<usize>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_write_tokens: u64,
    pub cache_read_tokens: u64,
}

/// End-of-session token totals: a row per agent (main and any
/// sub-agents) plus a grand total.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub main_agent_usage: SubAgentUsage,
    pub sub_agent_usage: Vec<SubAgentUsage>,
    pub total_usage: SubAgentUsage,
}
