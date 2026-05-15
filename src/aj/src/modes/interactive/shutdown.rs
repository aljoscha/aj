//! End-of-session shutdown banner: token usage summary plus
//! resume hint.
//!
//! Prints a per-agent token-usage breakdown (main + each
//! sub-agent + grand total) followed by a `Thread: <id> (resume
//! with: aj continue <id>)` line so the user can pick the
//! conversation back up later.
//!
//! Both lines are printed to stdout *after* [`aj_tui::tui::Tui::stop`]
//! so the bytes land in the user's regular shell scrollback rather
//! than the alternate-screen TUI buffer that gets cleared on exit.
//! `aj_tui::style::dim` adds an ANSI dim attribute so the banner
//! sits visually below whatever the user's normal terminal output
//! looks like.

use std::collections::HashMap;

use aj_agent::Agent;
use aj_agent::types::{SubAgentUsage, UsageSummary};
use aj_models::messages::Usage;
use aj_tui::style;

/// Compute the structured end-of-session token-usage summary from
/// the agent's accumulated counters and per-sub-agent breakdown.
///
/// Thin wrapper around [`build_usage_summary_from_parts`] that
/// reads the parts off the agent. Split so unit tests can build
/// summaries from primitive [`Usage`] values without needing to
/// construct a live [`Agent`].
pub fn build_usage_summary(agent: &Agent) -> UsageSummary {
    build_usage_summary_from_parts(agent.accumulated_usage(), agent.sub_agent_usage())
}

/// Project a main-agent [`Usage`] plus a `HashMap` of sub-agent
/// usages onto a [`UsageSummary`].
///
/// Sub-agent rows are emitted in ascending `agent_id` order for
/// deterministic output (the underlying `HashMap` doesn't
/// guarantee iteration order, and the legacy binary sorts here
/// for the same reason). Cache-creation / cache-read counts on
/// the wire `Usage` are `Option<u64>`; absent values flatten to
/// `0` in the summary's `u64` fields.
pub fn build_usage_summary_from_parts(main: &Usage, subs: &HashMap<usize, Usage>) -> UsageSummary {
    let main_agent_usage = SubAgentUsage {
        agent_id: None,
        input_tokens: main.input_tokens,
        output_tokens: main.output_tokens,
        cache_creation_tokens: main.cache_creation_input_tokens.unwrap_or(0),
        cache_read_tokens: main.cache_read_input_tokens.unwrap_or(0),
    };

    // Sort by id so the rendered table is stable across runs.
    let mut ordered: Vec<(usize, &Usage)> = subs.iter().map(|(id, u)| (*id, u)).collect();
    ordered.sort_by_key(|(id, _)| *id);

    let mut sub_agent_usage = Vec::with_capacity(ordered.len());
    let mut total_sub_input = 0u64;
    let mut total_sub_output = 0u64;
    let mut total_sub_cache_creation = 0u64;
    let mut total_sub_cache_read = 0u64;
    for (agent_id, usage) in ordered {
        let row = SubAgentUsage {
            agent_id: Some(agent_id),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
            cache_read_tokens: usage.cache_read_input_tokens.unwrap_or(0),
        };
        total_sub_input += row.input_tokens;
        total_sub_output += row.output_tokens;
        total_sub_cache_creation += row.cache_creation_tokens;
        total_sub_cache_read += row.cache_read_tokens;
        sub_agent_usage.push(row);
    }

    let total_usage = SubAgentUsage {
        agent_id: None,
        input_tokens: main_agent_usage.input_tokens + total_sub_input,
        output_tokens: main_agent_usage.output_tokens + total_sub_output,
        cache_creation_tokens: main_agent_usage.cache_creation_tokens + total_sub_cache_creation,
        cache_read_tokens: main_agent_usage.cache_read_tokens + total_sub_cache_read,
    };

    UsageSummary {
        main_agent_usage,
        sub_agent_usage,
        total_usage,
    }
}

/// Format a [`UsageSummary`] into the canonical multi-line block
/// the legacy `aj` binary prints at end-of-session: one row per
/// agent (`Main Agent` first, `Sub-agent <n>` rows in `agent_id`
/// order), then a trailing `TOTAL` row. No trailing newline — the
/// caller adds one when printing.
///
/// The per-row shape `Input: A | Output: B | Cache Creation: C |
/// Cache Read: D` matches the legacy binary's output byte-for-byte
/// so users who script against either binary see the same numbers
/// in the same positions.
pub fn format_usage_summary(summary: &UsageSummary) -> String {
    let format_row = |usage: &SubAgentUsage| -> String {
        format!(
            "Input: {} | Output: {} | Cache Creation: {} | Cache Read: {}",
            usage.input_tokens,
            usage.output_tokens,
            usage.cache_creation_tokens,
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
    out
}

/// Build the resume-hint line for the given thread id.
///
/// Exposed as a pure formatter so tests can lock the exact shape
/// without spawning a TUI. The runtime helper
/// [`print_resume_hint`] wraps this in an ANSI dim style and emits
/// it to stdout.
pub fn format_resume_hint(thread_id: &str) -> String {
    format!("Thread: {thread_id} (resume with: aj continue {thread_id})")
}

/// Print the end-of-session usage summary to stdout, dimmed to
/// match the legacy `aj` binary's visual cue. Intended to be
/// called after [`aj_tui::tui::Tui::stop`] so the bytes land in
/// the user's regular shell scrollback.
pub fn print_usage_summary(summary: &UsageSummary) {
    // Trailing blank line mirrors the legacy `display_token_usage_summary`,
    // which printed `"{}\n"` (i.e. one extra newline after the
    // formatted block) so the resume hint that follows has a
    // visual gap above it.
    println!("{}\n", style::dim(&format_usage_summary(summary)));
}

/// Print the resume hint to stdout, dimmed. Called only when the
/// thread has at least one persisted user message (otherwise the
/// hint points at an effectively-empty thread and isn't worth
/// surfacing).
pub fn print_resume_hint(thread_id: &str) {
    println!("{}\n", style::dim(&format_resume_hint(thread_id)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a [`Usage`] with explicit values for the four
    /// dimensions the summary cares about. `Default::default` for
    /// the fields we don't exercise (iterations, server tool use,
    /// etc.) — those don't surface in the end-of-session block.
    fn usage(
        input: u64,
        output: u64,
        cache_creation: Option<u64>,
        cache_read: Option<u64>,
    ) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: cache_creation,
            cache_read_input_tokens: cache_read,
            ..Usage::default()
        }
    }

    #[test]
    fn build_usage_summary_with_no_subagents_zeros_sub_rows() {
        let main = usage(100, 50, Some(10), Some(5));
        let summary = build_usage_summary_from_parts(&main, &HashMap::new());

        assert!(summary.sub_agent_usage.is_empty());
        assert_eq!(summary.main_agent_usage.input_tokens, 100);
        assert_eq!(summary.main_agent_usage.output_tokens, 50);
        assert_eq!(summary.main_agent_usage.cache_creation_tokens, 10);
        assert_eq!(summary.main_agent_usage.cache_read_tokens, 5);

        // Total equals main when there are no sub-agents.
        assert_eq!(summary.total_usage.input_tokens, 100);
        assert_eq!(summary.total_usage.output_tokens, 50);
        assert_eq!(summary.total_usage.cache_creation_tokens, 10);
        assert_eq!(summary.total_usage.cache_read_tokens, 5);
    }

    #[test]
    fn build_usage_summary_sorts_subagents_by_id_and_sums_totals() {
        let main = usage(100, 50, Some(10), Some(5));
        let mut subs = HashMap::new();
        // Insert out of order to verify sorting.
        subs.insert(3usize, usage(7, 3, Some(1), Some(2)));
        subs.insert(1usize, usage(20, 10, None, Some(4)));
        subs.insert(2usize, usage(30, 15, Some(2), None));
        let summary = build_usage_summary_from_parts(&main, &subs);

        let ids: Vec<_> = summary
            .sub_agent_usage
            .iter()
            .map(|row| row.agent_id.unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2, 3]);

        // Total = main + sum(subs); absent cache_* flatten to 0.
        assert_eq!(summary.total_usage.input_tokens, 100 + 20 + 30 + 7);
        assert_eq!(summary.total_usage.output_tokens, 50 + 10 + 15 + 3);
        assert_eq!(summary.total_usage.cache_creation_tokens, 10 + 0 + 2 + 1);
        assert_eq!(summary.total_usage.cache_read_tokens, 5 + 4 + 0 + 2);
    }

    #[test]
    fn format_usage_summary_renders_main_only_block() {
        let summary = UsageSummary {
            main_agent_usage: SubAgentUsage {
                agent_id: None,
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 10,
                cache_read_tokens: 5,
            },
            sub_agent_usage: Vec::new(),
            total_usage: SubAgentUsage {
                agent_id: None,
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 10,
                cache_read_tokens: 5,
            },
        };
        let expected = "Main Agent - Input: 100 | Output: 50 | Cache Creation: 10 | Cache Read: 5\n\
             TOTAL - Input: 100 | Output: 50 | Cache Creation: 10 | Cache Read: 5";
        assert_eq!(format_usage_summary(&summary), expected);
    }

    #[test]
    fn format_usage_summary_renders_subagent_rows_in_order() {
        let summary = UsageSummary {
            main_agent_usage: SubAgentUsage {
                agent_id: None,
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
            sub_agent_usage: vec![
                SubAgentUsage {
                    agent_id: Some(1),
                    input_tokens: 20,
                    output_tokens: 10,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                },
                SubAgentUsage {
                    agent_id: Some(2),
                    input_tokens: 30,
                    output_tokens: 15,
                    cache_creation_tokens: 0,
                    cache_read_tokens: 0,
                },
            ],
            total_usage: SubAgentUsage {
                agent_id: None,
                input_tokens: 150,
                output_tokens: 75,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
            },
        };
        let expected = "Main Agent - Input: 100 | Output: 50 | Cache Creation: 0 | Cache Read: 0\n\
             Sub-agent 1 - Input: 20 | Output: 10 | Cache Creation: 0 | Cache Read: 0\n\
             Sub-agent 2 - Input: 30 | Output: 15 | Cache Creation: 0 | Cache Read: 0\n\
             TOTAL - Input: 150 | Output: 75 | Cache Creation: 0 | Cache Read: 0";
        assert_eq!(format_usage_summary(&summary), expected);
    }

    #[test]
    fn format_resume_hint_round_trips_thread_id() {
        let hint = format_resume_hint("abc123");
        assert_eq!(hint, "Thread: abc123 (resume with: aj continue abc123)");
    }
}
