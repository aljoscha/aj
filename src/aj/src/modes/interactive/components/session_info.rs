//! Read-only session-info overlay (`/session info`, shown as "session
//! info" in the palette).
//!
//! Renders a [`SessionStats`] digest of the current session: identity
//! (id, on-disk path, project), recorded settings, activity timing, the
//! message counts broken out by kind, and the per-tool call breakdown.
//! The rows are grouped into labelled sections. The list and close-key
//! mechanics, plus scrolling for a tall digest, are the shared
//! [`ReadOnlyListOverlay`]. This module only builds the rows.

use aj_app::session_info::{InfoRow, digest};
use aj_session::SessionStats;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};

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
    let items = render_rows(&digest(&stats));
    // `ReadOnlyListOverlay` windows and scrolls the rows itself and sizes
    // the list's window to the item count, so this seed value is overridden.
    let visible = items.len().max(1);
    let scroll_info = std::sync::Arc::clone(&list_theme.scroll_info);
    let list = SelectList::new(items, visible, list_theme, layout);
    ReadOnlyListOverlay::new(list, scroll_info)
}

/// Convert the shared digest into list items, aligning every key/value
/// pair against one shared key column so the values line up across
/// sections.
fn render_rows(rows: &[InfoRow]) -> Vec<SelectItem> {
    let key_width = rows
        .iter()
        .filter_map(|row| match row {
            InfoRow::Kv { key, .. } => Some(key.chars().count()),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    rows.iter()
        .map(|row| match row {
            InfoRow::Header(title) => SelectItem::new("", title),
            InfoRow::Kv { key, value } => {
                // Two-space indent under the section header, then the key
                // padded to the shared column, then the value. No prefix
                // or description column is set, so the value uses the full
                // row width and a long path is not clipped to 32 columns.
                let label = format!("  {key:<key_width$}  {value}");
                SelectItem::new("", &label)
            }
            InfoRow::Blank => SelectItem::new("", ""),
        })
        .collect()
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
        let body = c
            .render(120)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
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
