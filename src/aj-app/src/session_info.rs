//! Frontend-agnostic session-info digest.
//!
//! Turns a [`SessionStats`] into an ordered list of [`InfoRow`]s:
//! labelled sections, key/value pairs, and blank spacers between
//! sections. The frontend renders these rows as styled spans; keeping
//! the digest here keeps its content independent of how it is drawn.

use aj_session::{SessionStats, UsageBucket};
use chrono::{DateTime, Utc};

/// One digest row: a section header, a key/value pair, or a blank
/// spacer between sections.
pub enum InfoRow {
    Header(String),
    Kv { key: String, value: String },
    Blank,
}

fn kv(key: &str, value: &str) -> InfoRow {
    InfoRow::Kv {
        key: key.to_string(),
        value: value.to_string(),
    }
}

/// Build the session-info digest: identity, recorded settings, activity
/// timing, message counts, aggregate usage, its per-provider/model/account
/// usage breakdown, and the per-tool call breakdown, grouped into labelled
/// sections separated by blank rows.
///
/// `tag` is the label the session carries, which lives beside the log rather
/// than in it, so the caller supplies it.
pub fn digest(stats: &SessionStats, tag: Option<&str>) -> Vec<InfoRow> {
    let total_messages = stats.user_messages + stats.assistant_messages + stats.tool_results;

    let mut rows: Vec<InfoRow> = vec![
        InfoRow::Header("Session".to_string()),
        kv("id", &stats.session_id),
        kv("tag", tag.unwrap_or("(none)")),
        kv("file", &stats.path.display().to_string()),
        kv("project", &project_name(stats)),
        InfoRow::Blank,
        InfoRow::Header("Settings".to_string()),
        kv("model", &model_label(stats)),
        kv(
            "thinking",
            stats.settings.thinking.as_deref().unwrap_or("(default)"),
        ),
        kv(
            "speed",
            stats.settings.speed.as_deref().unwrap_or("(default)"),
        ),
        kv(
            "verbosity",
            stats.settings.verbosity.as_deref().unwrap_or("(default)"),
        ),
        InfoRow::Blank,
        InfoRow::Header("Activity".to_string()),
        kv("created", &timestamp(stats.created_at, "(unknown)")),
        kv("last activity", &timestamp(stats.last_activity, "(none)")),
        kv("size on disk", &size_label(stats.size_bytes)),
        InfoRow::Blank,
        InfoRow::Header("Messages".to_string()),
        kv("total", &total_messages.to_string()),
        kv("user", &stats.user_messages.to_string()),
        kv("assistant", &stats.assistant_messages.to_string()),
        kv("tool results", &stats.tool_results.to_string()),
        kv("sub-agents", &stats.subagents.to_string()),
        kv("compactions", &stats.compactions.to_string()),
        kv("log entries", &stats.total_entries.to_string()),
        InfoRow::Blank,
        InfoRow::Header("Usage".to_string()),
        kv("input", &stats.usage.input.to_string()),
        kv("output", &stats.usage.output.to_string()),
        kv("cache read", &stats.usage.cache_read.to_string()),
        kv("cache write", &stats.usage.cache_write.to_string()),
        kv("total tokens", &stats.usage.total_tokens.to_string()),
        kv("cost", &cost_label(stats.usage.cost.total)),
        kv("of which compaction", &compaction_label(stats)),
    ];

    for bucket in &stats.usage_breakdown {
        rows.push(kv(&bucket_key(bucket), &bucket_value(bucket)));
    }

    rows.push(InfoRow::Blank);
    rows.push(InfoRow::Header(format!(
        "Tool calls ({})",
        stats.tool_calls
    )));

    if stats.tool_call_counts.is_empty() {
        rows.push(kv("(none)", ""));
    } else {
        for (name, count) in &stats.tool_call_counts {
            rows.push(kv(name, &count.to_string()));
        }
    }

    rows
}

/// Project name = the per-project sessions directory the file lives in
/// (`~/.aj/sessions/<project>/<id>.jsonl`). Derived from the path since
/// the log itself does not carry it.
fn project_name(stats: &SessionStats) -> String {
    stats
        .path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .unwrap_or("(unknown)")
        .to_string()
}

fn model_label(stats: &SessionStats) -> String {
    match &stats.settings.model {
        Some((provider, model_id)) => format!("{provider} / {model_id}"),
        None => "(unset)".to_string(),
    }
}

fn timestamp(value: Option<DateTime<Utc>>, fallback: &str) -> String {
    match value {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
        None => fallback.to_string(),
    }
}

fn size_label(bytes: Option<u64>) -> String {
    match bytes {
        None => "(not written yet)".to_string(),
        Some(b) if b < 1024 => format!("{b} B"),
        Some(b) if b < 1024 * 1024 => format!("{} KB", b / 1024),
        Some(b) => format!("{} MB", b / (1024 * 1024)),
    }
}

/// Format a recorded dollar cost to four decimal places.
///
/// Four places keep a sub-cent amount visible, matching the HTML export's
/// cost line.
fn cost_label(total: f64) -> String {
    format!("${total:.4}")
}

fn bucket_key(bucket: &UsageBucket) -> String {
    let provider = without_control_characters(&bucket.provider);
    let model = without_control_characters(&bucket.model);
    let key = format!("{provider} / {model}");
    match &bucket.account {
        Some(account) => format!("{key} ({})", without_control_characters(account)),
        None => key,
    }
}

fn without_control_characters(value: &str) -> String {
    value.chars().filter(|c| !c.is_control()).collect()
}

fn bucket_value(bucket: &UsageBucket) -> String {
    format!(
        "{} tokens · {}",
        bucket.usage.total_tokens,
        cost_label(bucket.usage.cost.total)
    )
}

/// The compaction share of the session's spend, as runs, tokens and
/// dollars.
///
/// Compaction is the one cost with no message behind it, so without a
/// line of its own it is spend the reader cannot attribute to anything
/// they remember doing. Runs whose spend was never recorded are named
/// separately rather than folded in, because a subtotal that silently
/// covers some of the runs reads as if it covered all of them.
fn compaction_label(stats: &SessionStats) -> String {
    let runs = stats.compactions;
    if runs == 0 {
        return "(none)".to_string();
    }
    let plural = if runs == 1 { "run" } else { "runs" };
    let missing = runs.saturating_sub(stats.compactions_with_usage);
    if stats.compactions_with_usage == 0 {
        return format!("{runs} {plural}, not recorded");
    }
    let usage = &stats.compaction_usage;
    let recorded = format!(
        "{runs} {plural}, {} tokens, {}",
        usage.total_tokens,
        cost_label(usage.cost.total)
    );
    if missing == 0 {
        recorded
    } else {
        format!("{recorded} ({missing} not recorded)")
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use aj_models::types::{Usage, UsageCost};
    use aj_session::SessionSettings;

    use super::*;

    fn sample_stats() -> SessionStats {
        SessionStats {
            session_id: "2026-06-19-14-22-03-512".to_string(),
            path: PathBuf::from("/home/u/.aj/sessions/home-u-proj/2026-06-19-14-22-03-512.jsonl"),
            created_at: None,
            last_activity: None,
            size_bytes: Some(48 * 1024),
            total_entries: 127,
            user_messages: 15,
            assistant_messages: 18,
            tool_results: 30,
            tool_calls: 31,
            tool_call_counts: vec![("read_file".to_string(), 12), ("Bash".to_string(), 8)],
            subagents: 2,
            compactions: 1,
            usage: Usage {
                input: 1_000,
                output: 2_000,
                cache_read: 500,
                cache_write: 250,
                total_tokens: 3_750,
                cost: UsageCost {
                    input: 0.10,
                    output: 0.20,
                    cache_read: 0.01,
                    cache_write: 0.02,
                    total: 0.33,
                },
            },
            usage_breakdown: vec![
                UsageBucket {
                    provider: "anthropic".to_string(),
                    model: "claude-sonnet-4-5".to_string(),
                    account: None,
                    usage: Usage {
                        input: 700,
                        output: 1_200,
                        cache_read: 300,
                        cache_write: 50,
                        total_tokens: 2_250,
                        cost: UsageCost {
                            input: 0.08,
                            output: 0.18,
                            cache_read: 0.02,
                            cache_write: 0.02,
                            total: 0.30,
                        },
                    },
                    responses: 12,
                    unpriced_responses: 0,
                },
                UsageBucket {
                    provider: "open\nai".to_string(),
                    model: "gpt-\r5".to_string(),
                    account: Some("wo\u{7}rk".to_string()),
                    usage: Usage {
                        input: 300,
                        output: 800,
                        cache_read: 200,
                        cache_write: 200,
                        total_tokens: 1_500,
                        cost: UsageCost {
                            input: 0.02,
                            output: 0.005,
                            cache_read: 0.003,
                            cache_write: 0.002,
                            total: 0.03,
                        },
                    },
                    responses: 6,
                    unpriced_responses: 0,
                },
            ],
            compaction_usage: Usage::default(),
            compactions_with_usage: 0,
            settings: SessionSettings {
                model: Some(("anthropic".to_string(), "claude-sonnet-4-5".to_string())),
                thinking: Some("medium".to_string()),
                speed: None,
                verbosity: None,
            },
        }
    }

    /// Render the digest to a `(kind, key, value)` view so the order,
    /// section headers, formatted values, and the blank spacers between
    /// sections can all be asserted from one pass.
    #[derive(Debug, PartialEq)]
    enum RowView {
        Header(String),
        Kv(String, String),
        Blank,
    }

    fn view(rows: &[InfoRow]) -> Vec<RowView> {
        rows.iter()
            .map(|r| match r {
                InfoRow::Header(t) => RowView::Header(t.clone()),
                InfoRow::Kv { key, value } => RowView::Kv(key.clone(), value.clone()),
                InfoRow::Blank => RowView::Blank,
            })
            .collect()
    }

    #[test]
    fn digest_sections_values_and_spacers_in_order() {
        let rows = view(&digest(&sample_stats(), Some("fix-auth")));

        // The section headers appear in order, each preceded by a blank
        // spacer once the first section is done.
        let expected = [
            RowView::Header("Session".to_string()),
            RowView::Kv("id".to_string(), "2026-06-19-14-22-03-512".to_string()),
            RowView::Kv("tag".to_string(), "fix-auth".to_string()),
            RowView::Kv(
                "file".to_string(),
                "/home/u/.aj/sessions/home-u-proj/2026-06-19-14-22-03-512.jsonl".to_string(),
            ),
            RowView::Kv("project".to_string(), "home-u-proj".to_string()),
            RowView::Blank,
            RowView::Header("Settings".to_string()),
            RowView::Kv(
                "model".to_string(),
                "anthropic / claude-sonnet-4-5".to_string(),
            ),
            RowView::Kv("thinking".to_string(), "medium".to_string()),
            RowView::Kv("speed".to_string(), "(default)".to_string()),
            RowView::Kv("verbosity".to_string(), "(default)".to_string()),
            RowView::Blank,
            RowView::Header("Activity".to_string()),
            RowView::Kv("created".to_string(), "(unknown)".to_string()),
            RowView::Kv("last activity".to_string(), "(none)".to_string()),
            RowView::Kv("size on disk".to_string(), "48 KB".to_string()),
            RowView::Blank,
            RowView::Header("Messages".to_string()),
            RowView::Kv("total".to_string(), "63".to_string()),
            RowView::Kv("user".to_string(), "15".to_string()),
            RowView::Kv("assistant".to_string(), "18".to_string()),
            RowView::Kv("tool results".to_string(), "30".to_string()),
            RowView::Kv("sub-agents".to_string(), "2".to_string()),
            RowView::Kv("compactions".to_string(), "1".to_string()),
            RowView::Kv("log entries".to_string(), "127".to_string()),
            RowView::Blank,
            RowView::Header("Usage".to_string()),
            RowView::Kv("input".to_string(), "1000".to_string()),
            RowView::Kv("output".to_string(), "2000".to_string()),
            RowView::Kv("cache read".to_string(), "500".to_string()),
            RowView::Kv("cache write".to_string(), "250".to_string()),
            RowView::Kv("total tokens".to_string(), "3750".to_string()),
            RowView::Kv("cost".to_string(), "$0.3300".to_string()),
            RowView::Kv(
                "of which compaction".to_string(),
                "1 run, not recorded".to_string(),
            ),
            RowView::Kv(
                "anthropic / claude-sonnet-4-5".to_string(),
                "2250 tokens · $0.3000".to_string(),
            ),
            RowView::Kv(
                "openai / gpt-5 (work)".to_string(),
                "1500 tokens · $0.0300".to_string(),
            ),
            RowView::Blank,
            RowView::Header("Tool calls (31)".to_string()),
            RowView::Kv("read_file".to_string(), "12".to_string()),
            RowView::Kv("Bash".to_string(), "8".to_string()),
        ];
        assert_eq!(rows, expected);
    }

    /// The compaction line reports the recorded spend, and says how many
    /// runs it does not cover rather than letting a partial subtotal
    /// read as a complete one.
    #[test]
    fn the_compaction_line_separates_recorded_runs_from_unrecorded_ones() {
        let mut stats = sample_stats();
        stats.compactions = 3;
        stats.compactions_with_usage = 2;
        stats.compaction_usage = Usage {
            total_tokens: 40_900,
            cost: UsageCost {
                total: 0.25,
                ..UsageCost::default()
            },
            ..Usage::default()
        };
        assert_eq!(
            value_of(&digest(&stats, None), "of which compaction"),
            "3 runs, 40900 tokens, $0.2500 (1 not recorded)"
        );

        stats.compactions_with_usage = 3;
        assert_eq!(
            value_of(&digest(&stats, None), "of which compaction"),
            "3 runs, 40900 tokens, $0.2500",
            "nothing missing, nothing to qualify"
        );

        stats.compactions = 0;
        stats.compactions_with_usage = 0;
        assert_eq!(
            value_of(&digest(&stats, None), "of which compaction"),
            "(none)"
        );
    }

    fn value_of(rows: &[InfoRow], key: &str) -> String {
        rows.iter()
            .find_map(|r| match r {
                InfoRow::Kv { key: k, value } if k == key => Some(value.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no {key} row"))
    }

    /// An untagged session says so rather than dropping the row, so the page's
    /// shape does not depend on whether a label happens to be set.
    #[test]
    fn an_untagged_session_reads_as_none() {
        let rows = view(&digest(&sample_stats(), None));
        assert!(
            rows.contains(&RowView::Kv("tag".to_string(), "(none)".to_string())),
            "{rows:?}",
        );
    }
}
