//! Display-oriented data types carried on bus events.
//!
//! [`TokenUsage`], [`SubAgentUsage`], and [`UsageSummary`] are
//! structured token-count snapshots the renderer formats.
//! [`TokenUsage`] rides on [`crate::events::AgentEvent::UsageUpdate`]
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
/// shape (`input`, `output`, `cache_read`, `cache_write`).
///
/// Polling [`crate::Agent::accumulated_usage`] *between* turns
/// returns the post-add total (i.e. the next `UsageUpdate` event's
/// `accumulated_* + turn_*`), so a consumer that needs the
/// "current running total at any instant" can either read the
/// getter or maintain its own sum off the bus events. The two completeness
/// fields preserve the same pre-add split for disclosure gaps.
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
    /// Whether the turn's numeric fields are only a recorded subtotal.
    #[serde(default, skip_serializing_if = "bool_is_false")]
    pub turn_incomplete: bool,
    /// Whether an earlier successful turn in the pre-add total was partial.
    #[serde(default, skip_serializing_if = "bool_is_false")]
    pub accumulated_incomplete: bool,
}

fn bool_is_false(value: &bool) -> bool {
    !*value
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
/// sub-agents) plus a grand total and one aggregate disclosure state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageSummary {
    pub main_agent_usage: SubAgentUsage,
    pub sub_agent_usage: Vec<SubAgentUsage>,
    pub total_usage: SubAgentUsage,
    /// Whether any row contributes only recorded usage rather than a complete
    /// provider disclosure.
    #[serde(default, skip_serializing_if = "bool_is_false")]
    pub incomplete: bool,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn completeness_fields_are_additive_on_usage_events_and_summaries() {
        let legacy_usage = json!({
            "accumulated_input": 10,
            "turn_input": 2,
            "accumulated_output": 3,
            "turn_output": 4,
            "accumulated_cache_write": 5,
            "turn_cache_write": 6,
            "accumulated_cache_read": 7,
            "turn_cache_read": 8
        });
        let usage: TokenUsage =
            serde_json::from_value(legacy_usage.clone()).expect("legacy usage event decodes");
        assert!(!usage.turn_incomplete);
        assert!(!usage.accumulated_incomplete);
        assert_eq!(
            serde_json::to_value(&usage).expect("legacy usage event re-encodes"),
            legacy_usage
        );

        let mut current_usage = usage;
        current_usage.turn_incomplete = true;
        current_usage.accumulated_incomplete = true;
        let encoded = serde_json::to_value(current_usage).expect("current usage event encodes");
        assert_eq!(encoded["turn_incomplete"], true);
        assert_eq!(encoded["accumulated_incomplete"], true);

        let legacy_summary = json!({
            "main_agent_usage": {
                "agent_id": null,
                "input_tokens": 1,
                "output_tokens": 2,
                "cache_write_tokens": 3,
                "cache_read_tokens": 4
            },
            "sub_agent_usage": [],
            "total_usage": {
                "agent_id": null,
                "input_tokens": 1,
                "output_tokens": 2,
                "cache_write_tokens": 3,
                "cache_read_tokens": 4
            }
        });
        let mut summary: UsageSummary =
            serde_json::from_value(legacy_summary.clone()).expect("legacy summary decodes");
        assert!(!summary.incomplete);
        assert_eq!(
            serde_json::to_value(&summary).expect("legacy summary re-encodes"),
            legacy_summary
        );
        summary.incomplete = true;
        let current = serde_json::to_value(summary).expect("current summary encodes");
        assert_eq!(current["incomplete"], true);
        assert!(current["main_agent_usage"].get("incomplete").is_none());
        assert!(current["total_usage"].get("incomplete").is_none());
    }
}
