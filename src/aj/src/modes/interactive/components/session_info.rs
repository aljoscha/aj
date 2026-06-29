//! Read-only session-info overlay (`/session info`, shown as "session
//! info" in the palette).
//!
//! Renders a [`SessionStats`] digest of the current session: identity
//! (id, on-disk path, project), recorded settings, activity timing, the
//! message counts broken out by kind, and the per-tool call breakdown.
//! The rows are grouped into labelled sections. The list and close-key
//! mechanics, plus scrolling for a tall digest, are the shared
//! [`ReadOnlyListOverlay`]. This module only builds the rows.

use aj_session::SessionStats;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use chrono::{DateTime, Utc};

use crate::modes::interactive::components::read_only_list::{
    ReadOnlyCloseHandle, ReadOnlyListOverlay,
};

/// Cheap-to-clone handle the host polls to learn the overlay was closed.
pub type SessionInfoOutcomeHandle = ReadOnlyCloseHandle;

/// Build a read-only session-info overlay from a pre-computed digest.
pub fn build_overlay(list_theme: SelectListTheme, stats: SessionStats) -> ReadOnlyListOverlay {
    let layout = SelectListLayout {
        show_selection_indicator: false,
        ..Default::default()
    };
    let items = build_items(&stats);
    // `ReadOnlyListOverlay` windows and scrolls the rows itself and sizes
    // the list's window to the item count, so this seed value is overridden.
    let visible = items.len().max(1);
    let scroll_info = std::sync::Arc::clone(&list_theme.scroll_info);
    let list = SelectList::new(items, visible, list_theme, layout);
    ReadOnlyListOverlay::new(list, scroll_info)
}

/// One rendered row: a section header, a key/value pair, or a spacer.
enum Row {
    Header(String),
    Kv { key: String, value: String },
    Blank,
}

fn build_items(stats: &SessionStats) -> Vec<SelectItem> {
    let total_messages = stats.user_messages + stats.assistant_messages + stats.tool_results;

    let mut rows: Vec<Row> = vec![
        Row::Header("Session".to_string()),
        kv("id", &stats.session_id),
        kv("file", &stats.path.display().to_string()),
        kv("project", &project_name(stats)),
        Row::Blank,
        Row::Header("Settings".to_string()),
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
        Row::Blank,
        Row::Header("Activity".to_string()),
        kv("created", &timestamp(stats.created_at, "(unknown)")),
        kv("last activity", &timestamp(stats.last_activity, "(none)")),
        kv("size on disk", &size_label(stats.size_bytes)),
        Row::Blank,
        Row::Header("Messages".to_string()),
        kv("total", &total_messages.to_string()),
        kv("user", &stats.user_messages.to_string()),
        kv("assistant", &stats.assistant_messages.to_string()),
        kv("tool results", &stats.tool_results.to_string()),
        kv("sub-agents", &stats.subagents.to_string()),
        kv("compactions", &stats.compactions.to_string()),
        kv("log entries", &stats.total_entries.to_string()),
        Row::Blank,
        Row::Header("Usage".to_string()),
        kv("input", &stats.usage.input.to_string()),
        kv("output", &stats.usage.output.to_string()),
        kv("cache read", &stats.usage.cache_read.to_string()),
        kv("cache write", &stats.usage.cache_write.to_string()),
        kv("total tokens", &stats.usage.total_tokens.to_string()),
        kv("cost", &cost_label(stats.usage.cost.total)),
        Row::Blank,
        Row::Header(format!("Tool calls ({})", stats.tool_calls)),
    ];

    if stats.tool_call_counts.is_empty() {
        rows.push(kv("(none)", ""));
    } else {
        for (name, count) in &stats.tool_call_counts {
            rows.push(kv(name, &count.to_string()));
        }
    }

    render_rows(&rows)
}

fn kv(key: &str, value: &str) -> Row {
    Row::Kv {
        key: key.to_string(),
        value: value.to_string(),
    }
}

/// Convert the row model into list items, aligning every key/value pair
/// against one shared key column so the values line up across sections.
fn render_rows(rows: &[Row]) -> Vec<SelectItem> {
    let key_width = rows
        .iter()
        .filter_map(|row| match row {
            Row::Kv { key, .. } => Some(key.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    rows.iter()
        .map(|row| match row {
            Row::Header(title) => SelectItem::new("", title),
            Row::Kv { key, value } => {
                // Two-space indent under the section header, then the key
                // padded to the shared column, then the value. No prefix
                // or description column is set, so the value uses the full
                // row width and a long path is not clipped to 32 columns.
                let label = format!("  {key:<key_width$}  {value}");
                SelectItem::new("", &label)
            }
            Row::Blank => SelectItem::new("", ""),
        })
        .collect()
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

/// Format the aggregate session cost as a dollar figure. Four decimal
/// places so a sub-cent session still shows a non-zero amount, matching
/// the HTML export's cost line.
fn cost_label(total: f64) -> String {
    format!("${total:.4}")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use aj_models::types::{Usage, UsageCost};
    use aj_session::SessionSettings;
    use aj_tui::component::Component;
    use aj_tui::keys::Key;

    use super::*;

    fn identity_theme() -> SelectListTheme {
        SelectListTheme {
            selected_prefix: Arc::new(|s| s.to_string()),
            selected_text: Arc::new(|s| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
            prefix: Arc::new(|s| s.to_string()),
            shortcut: Arc::new(|s| s.to_string()),
        }
    }

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
            settings: SessionSettings {
                model: Some(("anthropic".to_string(), "claude-sonnet-4-5".to_string())),
                thinking: Some("medium".to_string()),
                speed: None,
                verbosity: None,
            },
        }
    }

    #[test]
    fn renders_identity_counts_and_tool_breakdown() {
        let mut c = build_overlay(identity_theme(), sample_stats());
        let body = c.render(120).join("\n");
        assert!(body.contains("2026-06-19-14-22-03-512"), "{body}");
        assert!(body.contains("home-u-proj"), "{body}");
        assert!(body.contains("anthropic / claude-sonnet-4-5"), "{body}");
        // The full path is not clipped to the default 32-col primary width.
        assert!(body.contains("2026-06-19-14-22-03-512.jsonl"), "{body}");
        assert!(body.contains("48 KB"), "{body}");
        assert!(body.contains("read_file"), "{body}");
        assert!(body.contains("Tool calls (31)"), "{body}");
        // The usage section reports aggregate tokens and the dollar cost.
        assert!(body.contains("Usage"), "{body}");
        assert!(body.contains("total tokens"), "{body}");
        assert!(body.contains("$0.3300"), "{body}");
    }

    #[test]
    fn esc_and_enter_close() {
        let mut c = build_overlay(identity_theme(), sample_stats());
        let h = c.outcome_handle();
        c.handle_input(&Key::escape());
        assert!(h.take().is_some(), "Esc should close");

        let mut c = build_overlay(identity_theme(), sample_stats());
        let h = c.outcome_handle();
        c.handle_input(&Key::enter());
        assert!(h.take().is_some(), "Enter should close");
    }
}
