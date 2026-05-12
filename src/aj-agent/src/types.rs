//! Display-oriented data types carried on bus events and through
//! the on-disk log.
//!
//! These are the bridging shapes that ride on events crossing the
//! agent-to-listener boundary while the wire layer is still
//! mid-migration. [`UserOutput`] is the legacy on-disk shape for
//! freestanding tool errors (today only [`UserOutput::ToolError`] is
//! ever written; see `docs/aj-next-progress.md` §2.0 reconnaissance);
//! [`TokenUsage`], [`SubAgentUsage`], and [`UsageSummary`] are
//! structured token-count snapshots the renderer formats.
//!
//! Both shapes fold away in §2.4 of `docs/aj-next-plan.md`:
//! [`UserOutput`] becomes structured [`crate::tool::ToolDetails`]
//! entries on disk via the §3 migration walker; [`TokenUsage`] gets
//! subsumed by [`aj_models::types::AssistantMessage::usage`] riding
//! on [`crate::events::AgentEvent::MessageEnd`].

use serde::{Deserialize, Serialize};

/// Per-turn token-usage snapshot suitable for an at-a-glance
/// renderer. Carries both turn-local and accumulated counts so the
/// caller doesn't need to subtract.
///
/// The accumulator semantics match what the agent maintains in
/// [`crate::Agent::accumulated_usage`]: every successful turn adds
/// its [`aj_models::wire::Usage`] into the accumulator, and the
/// snapshot here is taken *after* that add, so `accumulated_*`
/// already includes the current turn's contribution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub accumulated_input: u64,
    pub turn_input: u64,
    pub accumulated_output: u64,
    pub turn_output: u64,
    pub accumulated_cache_creation: u64,
    pub turn_cache_creation: u64,
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
    pub cache_creation_tokens: u64,
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

/// Legacy "user-visible output" enum.
///
/// Today the agent only ever emits the [`UserOutput::ToolError`]
/// variant — the synthesized error record written when a tool's
/// `input` JSON fails to parse or the tool's `execute` itself
/// returns `Err`. The other variants survive only so the on-disk
/// format can deserialize older threads that recorded them
/// (per the §2.0 reconnaissance every freestanding `user_output`
/// entry on disk is a `ToolError`, but the type predates that
/// invariant). The §3 migration walker rewrites all surviving
/// `ToolError` records into structured [`crate::tool::ToolDetails`]
/// entries, after which `UserOutput` can drop entirely.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserOutput {
    /// Display a notice message to the user.
    Notice(String),
    /// Display an error message to the user.
    Error(String),
    /// Display the result of a tool execution.
    ToolResult {
        tool_name: String,
        input: String,
        output: String,
    },
    /// Display a diff showing before/after changes.
    ToolResultDiff {
        tool_name: String,
        input: String,
        before: String,
        after: String,
    },
    /// Display a tool error.
    ToolError {
        tool_name: String,
        input: String,
        error: String,
    },
    /// Display token usage information.
    TokenUsage(TokenUsage),
    /// Display token usage summary.
    TokenUsageSummary(UsageSummary),
}
