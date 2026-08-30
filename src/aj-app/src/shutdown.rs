//! Frontend-agnostic end-of-session usage math: project the agent's
//! accumulated counters into a [`UsageSummary`] and format the usage
//! block, resume hint, and per-session header lines.
//!
//! These are pure value-to-string formatters with no rendering concern.
//! A frontend calls them to compute the text, then styles and prints it
//! however it wants.

use std::collections::HashMap;

use aj_agent::Agent;
use aj_agent::types::{SubAgentUsage, UsageSummary};
use aj_models::types::Usage;

/// Compute the structured end-of-session token-usage summary from
/// the agent's accumulated counters and per-sub-agent breakdown.
///
/// Thin wrapper around [`build_usage_summary_from_parts`] that
/// reads the parts off the agent. Split so unit tests can build
/// summaries from primitive [`Usage`] values without needing to
/// construct a live [`Agent`].
pub fn build_usage_summary(agent: &Agent) -> UsageSummary {
    build_usage_summary_from_parts(&agent.accumulated_usage(), &agent.sub_agent_usage())
}

/// Project a main-agent [`Usage`] plus a `HashMap` of sub-agent
/// usages onto a [`UsageSummary`].
///
/// Sub-agent rows are emitted in ascending `agent_id` order for
/// deterministic output (the underlying `HashMap` doesn't
/// guarantee iteration order).
pub fn build_usage_summary_from_parts(main: &Usage, subs: &HashMap<usize, Usage>) -> UsageSummary {
    let main_agent_usage = SubAgentUsage {
        agent_id: None,
        input_tokens: main.input,
        output_tokens: main.output,
        cache_write_tokens: main.cache_write,
        cache_read_tokens: main.cache_read,
    };

    // Sort by id so the rendered table is stable across runs.
    let mut ordered: Vec<(usize, &Usage)> = subs.iter().map(|(id, u)| (*id, u)).collect();
    ordered.sort_by_key(|(id, _)| *id);

    let mut sub_agent_usage = Vec::with_capacity(ordered.len());
    let mut total_sub_input = 0u64;
    let mut total_sub_output = 0u64;
    let mut total_sub_cache_write = 0u64;
    let mut total_sub_cache_read = 0u64;
    let mut incomplete = main.incomplete;
    for (agent_id, usage) in ordered {
        let row = SubAgentUsage {
            agent_id: Some(agent_id),
            input_tokens: usage.input,
            output_tokens: usage.output,
            cache_write_tokens: usage.cache_write,
            cache_read_tokens: usage.cache_read,
        };
        total_sub_input += row.input_tokens;
        total_sub_output += row.output_tokens;
        total_sub_cache_write += row.cache_write_tokens;
        total_sub_cache_read += row.cache_read_tokens;
        incomplete |= usage.incomplete;
        sub_agent_usage.push(row);
    }

    let total_usage = SubAgentUsage {
        agent_id: None,
        input_tokens: main_agent_usage.input_tokens + total_sub_input,
        output_tokens: main_agent_usage.output_tokens + total_sub_output,
        cache_write_tokens: main_agent_usage.cache_write_tokens + total_sub_cache_write,
        cache_read_tokens: main_agent_usage.cache_read_tokens + total_sub_cache_read,
    };

    UsageSummary {
        main_agent_usage,
        sub_agent_usage,
        total_usage,
        incomplete,
    }
}

/// Format a [`UsageSummary`] into the canonical multi-line block
/// printed at end-of-session: one row per agent (`Main Agent` first,
/// `Sub-agent <n>` rows in `agent_id` order), then a trailing `TOTAL`
/// row. No trailing newline — the caller adds one when printing.
///
/// The per-row shape is `Input: A | Output: B | Cache Creation: C |
/// Cache Read: D`, a stable format users can script against.
pub fn format_usage_summary(summary: &UsageSummary) -> String {
    let format_row = |usage: &SubAgentUsage| -> String {
        format!(
            "Input: {} | Output: {} | Cache Creation: {} | Cache Read: {}",
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_write_tokens,
            usage.cache_read_tokens
        )
    };

    let mut out = String::new();
    out.push_str(&format!(
        "Main Agent - {}\n",
        format_row(&summary.main_agent_usage)
    ));
    for sub in &summary.sub_agent_usage {
        if let Some(id) = sub.agent_id {
            out.push_str(&format!("Sub-agent {} - {}\n", id, format_row(sub)));
        }
    }
    out.push_str(&format!("TOTAL - {}", format_row(&summary.total_usage)));
    if summary.incomplete {
        out.push_str("\nUsage: partial (recorded usage only)");
    }
    out
}

/// Build the resume-hint line for the given session id.
///
/// Exposed as a pure formatter so callers style and print it
/// however they like (the interactive frontend dims it and writes
/// it to stdout after the TUI stops).
pub fn format_resume_hint(session_id: &str) -> String {
    format!("Session: {session_id} (resume with: aj continue {session_id})")
}

/// Format the per-session header line printed above a usage block
/// when more than one session ran in the process. Exposed as a pure
/// formatter so callers can lock the exact shape.
pub fn format_session_usage_header(session_id: &str) -> String {
    format!("Session: {session_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a [`Usage`] with explicit values for the four
    /// dimensions the summary cares about. `Default::default` for
    /// the fields we don't exercise (cost, total_tokens) — those
    /// don't surface in the end-of-session block.
    fn usage(input: u64, output: u64, cache_write: u64, cache_read: u64) -> Usage {
        Usage {
            input,
            output,
            cache_write,
            cache_read,
            ..Usage::default()
        }
    }

    #[test]
    fn build_usage_summary_with_no_subagents_zeros_sub_rows() {
        let main = usage(100, 50, 10, 5);
        let summary = build_usage_summary_from_parts(&main, &HashMap::new());

        assert!(summary.sub_agent_usage.is_empty());
        assert_eq!(summary.main_agent_usage.input_tokens, 100);
        assert_eq!(summary.main_agent_usage.output_tokens, 50);
        assert_eq!(summary.main_agent_usage.cache_write_tokens, 10);
        assert_eq!(summary.main_agent_usage.cache_read_tokens, 5);

        assert_eq!(summary.total_usage.input_tokens, 100);
        assert_eq!(summary.total_usage.output_tokens, 50);
        assert_eq!(summary.total_usage.cache_write_tokens, 10);
        assert_eq!(summary.total_usage.cache_read_tokens, 5);
    }

    #[test]
    fn build_usage_summary_sorts_subagents_by_id_and_sums_totals() {
        let main = usage(100, 50, 10, 5);
        let mut subs = HashMap::new();
        // Insert out of order to verify sorting.
        subs.insert(3usize, usage(7, 3, 1, 2));
        subs.insert(1usize, usage(20, 10, 0, 4));
        let mut partial = usage(30, 15, 2, 0);
        partial.incomplete = true;
        subs.insert(2usize, partial);
        let summary = build_usage_summary_from_parts(&main, &subs);

        let ids: Vec<_> = summary
            .sub_agent_usage
            .iter()
            .map(|row| row.agent_id.unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);

        assert_eq!(summary.total_usage.input_tokens, 100 + 20 + 30 + 7);
        assert_eq!(summary.total_usage.output_tokens, 50 + 10 + 15 + 3);
        assert_eq!(summary.total_usage.cache_write_tokens, 10 + 0 + 2 + 1);
        assert_eq!(summary.total_usage.cache_read_tokens, 5 + 4 + 0 + 2);
        assert!(summary.incomplete);
    }

    #[test]
    fn format_usage_summary_renders_main_only_block() {
        let summary = UsageSummary {
            main_agent_usage: SubAgentUsage {
                agent_id: None,
                input_tokens: 100,
                output_tokens: 50,
                cache_write_tokens: 10,
                cache_read_tokens: 5,
            },
            sub_agent_usage: Vec::new(),
            total_usage: SubAgentUsage {
                agent_id: None,
                input_tokens: 100,
                output_tokens: 50,
                cache_write_tokens: 10,
                cache_read_tokens: 5,
            },
            incomplete: false,
        };
        let expected = "Main Agent - Input: 100 | Output: 50 | Cache Creation: 10 | Cache Read: 5\n\
             TOTAL - Input: 100 | Output: 50 | Cache Creation: 10 | Cache Read: 5";
        assert_eq!(format_usage_summary(&summary), expected);

        let mut partial = summary;
        partial.incomplete = true;
        assert_eq!(
            format_usage_summary(&partial),
            format!("{expected}\nUsage: partial (recorded usage only)")
        );
    }

    #[test]
    fn format_usage_summary_renders_subagent_rows_in_order() {
        let summary = UsageSummary {
            main_agent_usage: SubAgentUsage {
                agent_id: None,
                input_tokens: 100,
                output_tokens: 50,
                cache_write_tokens: 0,
                cache_read_tokens: 0,
            },
            sub_agent_usage: vec![
                SubAgentUsage {
                    agent_id: Some(1),
                    input_tokens: 20,
                    output_tokens: 10,
                    cache_write_tokens: 0,
                    cache_read_tokens: 0,
                },
                SubAgentUsage {
                    agent_id: Some(2),
                    input_tokens: 30,
                    output_tokens: 15,
                    cache_write_tokens: 0,
                    cache_read_tokens: 0,
                },
            ],
            total_usage: SubAgentUsage {
                agent_id: None,
                input_tokens: 150,
                output_tokens: 75,
                cache_write_tokens: 0,
                cache_read_tokens: 0,
            },
            incomplete: false,
        };
        let expected = "Main Agent - Input: 100 | Output: 50 | Cache Creation: 0 | Cache Read: 0\n\
             Sub-agent 1 - Input: 20 | Output: 10 | Cache Creation: 0 | Cache Read: 0\n\
             Sub-agent 2 - Input: 30 | Output: 15 | Cache Creation: 0 | Cache Read: 0\n\
             TOTAL - Input: 150 | Output: 75 | Cache Creation: 0 | Cache Read: 0";
        assert_eq!(format_usage_summary(&summary), expected);
    }

    #[test]
    fn format_session_usage_header_round_trips_session_id() {
        assert_eq!(format_session_usage_header("abc123"), "Session: abc123");
    }

    #[test]
    fn format_resume_hint_round_trips_session_id() {
        let hint = format_resume_hint("abc123");
        assert_eq!(hint, "Session: abc123 (resume with: aj continue abc123)");
    }
}
