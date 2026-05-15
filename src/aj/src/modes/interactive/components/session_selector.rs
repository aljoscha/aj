//! Session-selector overlay (`/session`).
//!
//! Pairs a [`aj_tui::components::text_input::TextInput`] for live
//! fuzzy filtering with a [`aj_tui::components::select_list::SelectList`]
//! that shows the matching entries from a snapshotted list of
//! [`aj_session::ThreadPreview`]s. The host opens this overlay from
//! `/session` (no args, full catalog) or `/session <query>` (pre-
//! filled search); `Enter` commits the highlighted thread, `Esc`
//! cancels.
//!
//! The component owns the catalog and rebuilds the inner
//! [`SelectList`] on every text change via a reusable
//! [`aj_tui::fuzzy::FuzzyMatcher`]. The currently-active thread is
//! pre-selected on open and tagged `(current)` so a no-op confirm is
//! visually obvious — the user can verify "yes, I'm staying on this
//! one" without scanning the file id.
//!
//! See `docs/aj-next-plan.md` Phase 1 §4 "Selectors and theming".

use std::sync::{Arc, Mutex};

use aj_session::ThreadPreview;
use aj_tui::component::Component;
use aj_tui::components::select_list::{SelectItem, SelectList, SelectListLayout, SelectListTheme};
use aj_tui::components::text_input::TextInput;
use aj_tui::fuzzy::FuzzyMatcher;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use aj_tui::style;
use chrono::{DateTime, Utc};

/// Maximum visible rows in the result list. Matches the model
/// selector's eight-row window so the two overlays feel
/// consistent; taller terminals see a scrolled view through
/// [`SelectList`]'s own scroll model.
const MAX_VISIBLE_ROWS: usize = 8;

/// Cap on how much of the first user message is rendered in the
/// primary column. Keeps long pastes (a stack trace, a chunk of
/// code) from blowing past the overlay width — the description
/// column carries the thread id and date so the user can still
/// disambiguate.
const PREVIEW_MAX_CHARS: usize = 80;

/// Outcome of a single overlay session.
///
/// `Confirmed(preview)` carries the chosen [`ThreadPreview`]
/// (cloned so the host can open the matching log without
/// borrowing the catalog); `Cancelled` is the user pressing `Esc`.
/// The host treats both as "close the overlay"; only the former
/// triggers the swap-thread flow.
#[derive(Debug, Clone)]
pub enum SessionSelectorOutcome {
    Confirmed(ThreadPreview),
    Cancelled,
}

/// Cheap-to-clone handle pointing at the same outcome slot the
/// overlay component writes into.
pub type OutcomeHandle = Arc<Mutex<Option<SessionSelectorOutcome>>>;

/// The overlay's top-level component.
///
/// Owns the search input (`search`), the inner [`SelectList`]
/// (`list`), the cached catalog (`catalog`), and the outcome slot
/// (`outcome`). The host keeps another clone of `outcome` and polls
/// it after every input event to decide whether to close the
/// overlay.
pub struct SessionSelectorComponent {
    /// Search box at the top of the overlay. Typing into it
    /// rebuilds `list`; Enter is intercepted at the component
    /// level so it commits the highlighted list item.
    search: TextInput,
    /// Result list. Rebuilt every time `search` changes so the
    /// fuzzy-filtered entries reflect the current query.
    list: SelectList,
    /// Full unfiltered catalog. The component clones the entry it
    /// emits on confirm; keeping the source of truth here avoids
    /// any chance of drift between filter and confirm.
    catalog: Vec<ThreadPreview>,
    /// Thread id of the agent's currently-active log. Used to
    /// pre-select that row on open and mark it `(current)` so a
    /// no-op confirm is obvious.
    current_thread_id: Option<String>,
    /// Shared outcome slot. The host clones this handle once at
    /// construction and polls it after every input event.
    outcome: OutcomeHandle,
    /// Theme used to build the inner [`SelectList`]. Stored so a
    /// rebuild (after a search-text change) can reuse the same
    /// palette without the host having to pass it back in.
    theme: SelectListTheme,
    /// Reusable fuzzy matcher. Pulled out as a field so we don't
    /// reconstruct the underlying nucleo state on every keystroke
    /// (it allocates ~135 KB up front per `FuzzyMatcher::new`).
    matcher: FuzzyMatcher,
    /// One-line title rendered above the search input.
    title: String,
    /// `now` snapshot taken at construction time. Used to format
    /// each row's age (`5m`, `3h`, …) without re-reading the clock
    /// on every rebuild. The selector closes within seconds in
    /// practice; "Just now" rows stay "Just now" for the whole
    /// session.
    now: DateTime<Utc>,
}

impl SessionSelectorComponent {
    /// Build a fresh selector.
    ///
    /// `catalog` is the snapshotted preview list (must already be
    /// sorted in the order the user should see when the query is
    /// empty; [`aj_session::ConversationPersistence::list_thread_previews`]
    /// returns latest-first already). `current_thread_id` is the
    /// agent's active thread — used to pre-select the matching row
    /// and mark it `(current)`. `initial_query` pre-fills the
    /// search box (used by `/session <query>`). `theme` styles the
    /// underlying [`SelectList`].
    pub fn new(
        theme: SelectListTheme,
        catalog: Vec<ThreadPreview>,
        current_thread_id: Option<String>,
        initial_query: Option<String>,
    ) -> Self {
        let mut search = TextInput::new("search: ");
        if let Some(q) = initial_query {
            search.set_value(&q);
        }
        search.set_focused(true);

        // Placeholder list — `rebuild_list` below replaces it with
        // the initial filter and pre-selection.
        let list = SelectList::new(
            Vec::new(),
            MAX_VISIBLE_ROWS,
            theme.clone(),
            primary_column_layout(),
        );

        let outcome: OutcomeHandle = Arc::new(Mutex::new(None));
        let mut component = Self {
            search,
            list,
            catalog,
            current_thread_id,
            outcome,
            theme,
            matcher: FuzzyMatcher::new(),
            title: "Resume thread — Enter to switch, Esc to cancel".to_string(),
            now: Utc::now(),
        };
        component.rebuild_list();
        component
    }

    /// Hand the host a clone of the outcome slot. After each input
    /// event the host calls `lock().take()` on this handle; on
    /// `Some(_)` it hides the overlay and applies the result.
    pub fn outcome_handle(&self) -> OutcomeHandle {
        Arc::clone(&self.outcome)
    }

    /// Rebuild `list` from `catalog` filtered by the current search
    /// value.
    ///
    /// Score policy: empty query returns the full catalog in its
    /// supplied order (the loader already sorts latest-first); a
    /// non-empty query fuzzy-scores against the preview's
    /// searchable blob (first user message + thread id) and sorts
    /// highest-score-first with catalog-order tiebreak.
    fn rebuild_list(&mut self) {
        let query = self.search.value().trim().to_string();
        let mut scored: Vec<(usize, u32)> = Vec::new();
        if query.is_empty() {
            scored.extend((0..self.catalog.len()).map(|i| (i, 0u32)));
        } else {
            for (idx, info) in self.catalog.iter().enumerate() {
                let haystack = haystack_for(info);
                if let Some(score) = self.matcher.score(&query, &haystack) {
                    scored.push((idx, u32::from(score)));
                }
            }
            scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        }

        let mut selected_index = 0;
        let items: Vec<SelectItem> = scored
            .iter()
            .enumerate()
            .map(|(row, (idx, _))| {
                let info = &self.catalog[*idx];
                let is_current = self
                    .current_thread_id
                    .as_ref()
                    .is_some_and(|tid| tid == &info.thread_id);
                if is_current {
                    selected_index = row;
                }
                let primary = format_primary(info, is_current);
                let secondary = format_secondary(info, self.now);
                SelectItem::new(&info.thread_id, &primary).with_description(&secondary)
            })
            .collect();

        let mut list = SelectList::new(
            items,
            MAX_VISIBLE_ROWS,
            self.theme.clone(),
            primary_column_layout(),
        );
        list.set_focused(true);
        list.set_selected_index(selected_index);
        self.list = list;
    }

    /// Commit the currently-highlighted list entry into the outcome
    /// slot. Looks the entry up in `catalog` by its `thread_id` to
    /// recover the full [`ThreadPreview`].
    fn commit_selection(&self) {
        let Some(item) = self.list.selected_item().cloned() else {
            return;
        };
        let Some(info) = self
            .catalog
            .iter()
            .find(|p| p.thread_id == item.value)
            .cloned()
        else {
            return;
        };
        *self.outcome.lock().expect("outcome mutex poisoned") =
            Some(SessionSelectorOutcome::Confirmed(info));
    }

    /// Record a cancellation in the outcome slot.
    fn commit_cancel(&self) {
        *self.outcome.lock().expect("outcome mutex poisoned") =
            Some(SessionSelectorOutcome::Cancelled);
    }
}

/// Searchable text for `preview`. Joins the first user message
/// (when present) and the thread id so typing either a substring
/// of the prompt or part of the timestamp finds the row.
fn haystack_for(preview: &ThreadPreview) -> String {
    let first = preview.first_user_message.as_deref().unwrap_or("");
    format!("{} {}", first, preview.thread_id)
}

/// Build the layout used for the inner [`SelectList`].
///
/// The default `SelectListLayout` allocates only 32 chars to the
/// primary column, which truncates our `<preview> (current)` rows.
/// Lift the cap to leave room for both the preview text and the
/// `(current)` suffix; the description column shrinks accordingly.
fn primary_column_layout() -> SelectListLayout {
    SelectListLayout {
        // PREVIEW_MAX_CHARS for the preview text + 10 chars for
        // " (current)" + 2-char inter-column gap that
        // `SelectList` accounts for inside this width.
        max_primary_column_width: Some(PREVIEW_MAX_CHARS + 12),
        ..SelectListLayout::default()
    }
}

/// Build the primary (left) column for one row. The first user
/// message is the most recognisable handle; thread id falls back
/// when none is captured. Truncated to keep long pastes from
/// blowing past the overlay width.
fn format_primary(preview: &ThreadPreview, is_current: bool) -> String {
    let raw = preview
        .first_user_message
        .as_deref()
        .unwrap_or("(no user message yet)");
    let one_line = raw.lines().next().unwrap_or(raw);
    let truncated = truncate_for_display(one_line, PREVIEW_MAX_CHARS);
    if is_current {
        format!("{truncated} (current)")
    } else {
        truncated
    }
}

/// Build the secondary (right / description) column for one row.
/// Carries thread metadata: human age and the message count. The
/// thread id itself is omitted — it's already the row's unique
/// value and would dominate the column width without adding much.
fn format_secondary(preview: &ThreadPreview, now: DateTime<Utc>) -> String {
    let age = format_age(now, preview.modified);
    let count = preview.message_count;
    let msg_word = if count == 1 { "msg" } else { "msgs" };
    format!("{count} {msg_word} · {age}")
}

/// Render `then` as a coarse age relative to `now`.
///
/// Buckets follow common UX conventions: `now / 5m / 3h / 2d /
/// 4w / 6mo / 2y`. The bucket boundaries are deliberately fuzzy —
/// a row showing `3h` may actually be 2h47m old, which is what the
/// user means by "a few hours ago".
fn format_age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let delta = now.signed_duration_since(then);
    let secs = delta.num_seconds().max(0);
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    let weeks = days / 7;
    let months = days / 30;
    let years = days / 365;

    if secs < 60 {
        "now".to_string()
    } else if mins < 60 {
        format!("{mins}m")
    } else if hours < 24 {
        format!("{hours}h")
    } else if days < 7 {
        format!("{days}d")
    } else if weeks < 4 {
        format!("{weeks}w")
    } else if months < 12 {
        format!("{months}mo")
    } else {
        format!("{years}y")
    }
}

/// Truncate `text` to fit in roughly `max_chars` display columns,
/// counting char positions (not bytes). Suffixes a trailing `…`
/// when truncation actually happened so the user knows there's
/// more.
fn truncate_for_display(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    // `max_chars - 1` to leave room for the ellipsis. The `min(..)`
    // guard keeps the slice index in bounds for the degenerate
    // `max_chars == 0` case.
    let cut = max_chars.saturating_sub(1).min(chars.len());
    let mut s: String = chars[..cut].iter().collect();
    s.push('…');
    s
}

impl Component for SessionSelectorComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        // Stack: title, search input, dim separator, list.
        let mut lines = Vec::with_capacity(MAX_VISIBLE_ROWS + 4);
        lines.push(style::dim(&self.title));
        lines.extend(self.search.render(width));
        lines.push(style::dim(&"─".repeat(width.min(60))));
        lines.extend(self.list.render(width));
        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        let kb = keybindings::get();

        // Esc cancels regardless of where focus appears to be.
        if kb.matches(event, "tui.select.cancel") {
            self.commit_cancel();
            return true;
        }

        // Enter commits the highlighted list row.
        if kb.matches(event, "tui.input.submit") {
            self.commit_selection();
            return true;
        }

        // Navigation keys belong to the list.
        if kb.matches(event, "tui.select.up")
            || kb.matches(event, "tui.select.down")
            || kb.matches(event, "tui.select.pageUp")
            || kb.matches(event, "tui.select.pageDown")
        {
            drop(kb);
            return self.list.handle_input(event);
        }

        // Everything else goes to the search box. Drop the
        // keybinding registry guard first so the rebuild below can
        // re-acquire it without contention.
        drop(kb);

        let before = self.search.value().to_string();
        let handled = self.search.handle_input(event);
        if handled && self.search.value() != before {
            self.rebuild_list();
        }
        handled
    }

    fn set_focused(&mut self, focused: bool) {
        self.search.set_focused(focused);
        self.list.set_focused(focused);
    }

    fn is_focused(&self) -> bool {
        self.search.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_tui::components::select_list::SelectListTheme;
    use aj_tui::keys::{InputEvent, Key};
    use chrono::Duration;

    use super::*;

    /// Identity theme for tests — passes every closure through
    /// verbatim so renders show structural text rather than ANSI.
    fn identity_theme() -> SelectListTheme {
        SelectListTheme {
            selected_prefix: Arc::new(|s| s.to_string()),
            selected_text: Arc::new(|s| s.to_string()),
            description: Arc::new(|s| s.to_string()),
            scroll_info: Arc::new(|s| s.to_string()),
            no_match: Arc::new(|s| s.to_string()),
        }
    }

    fn make_preview(
        thread_id: &str,
        first_user: Option<&str>,
        message_count: usize,
        age: Duration,
    ) -> ThreadPreview {
        ThreadPreview {
            thread_id: thread_id.to_string(),
            modified: Utc::now() - age,
            size_bytes: 1024,
            message_count,
            first_user_message: first_user.map(|s| s.to_string()),
        }
    }

    fn sample_catalog() -> Vec<ThreadPreview> {
        vec![
            make_preview(
                "2025-05-10",
                Some("refactor the agent loop"),
                42,
                Duration::minutes(15),
            ),
            make_preview(
                "2025-05-09",
                Some("debug the streaming protocol"),
                17,
                Duration::hours(3),
            ),
            make_preview(
                "2025-05-08",
                Some("add session selector"),
                8,
                Duration::days(2),
            ),
        ]
    }

    fn enter_event() -> InputEvent {
        Key::enter()
    }
    fn escape_event() -> InputEvent {
        Key::escape()
    }
    fn down_event() -> InputEvent {
        Key::down()
    }

    #[test]
    fn highlights_current_thread_on_open() {
        let catalog = sample_catalog();
        let mut sel = SessionSelectorComponent::new(
            identity_theme(),
            catalog,
            Some("2025-05-09".to_string()),
            None,
        );
        // Render wide enough that SelectList's primary column
        // doesn't truncate the `(current)` suffix off the label.
        let body = sel.render(200).join("\n");
        // The middle row should be marked "(current)".
        assert!(
            body.contains("debug the streaming protocol (current)"),
            "got: {body}"
        );
    }

    #[test]
    fn enter_commits_highlighted_entry() {
        let catalog = sample_catalog();
        let mut sel = SessionSelectorComponent::new(
            identity_theme(),
            catalog,
            Some("2025-05-10".to_string()),
            None,
        );
        let outcome = sel.outcome_handle();
        // 2025-05-10 is pre-selected (current); Enter should commit
        // it verbatim.
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            SessionSelectorOutcome::Confirmed(p) => {
                assert_eq!(p.thread_id, "2025-05-10");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn esc_emits_cancelled_outcome() {
        let catalog = sample_catalog();
        let mut sel = SessionSelectorComponent::new(identity_theme(), catalog, None, None);
        let outcome = sel.outcome_handle();
        sel.handle_input(&escape_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        assert!(
            matches!(result, SessionSelectorOutcome::Cancelled),
            "got {result:?}"
        );
    }

    #[test]
    fn down_arrow_moves_to_next_row_then_enter_confirms_it() {
        let catalog = sample_catalog();
        // No current thread → first row pre-selected.
        let mut sel = SessionSelectorComponent::new(identity_theme(), catalog, None, None);
        let outcome = sel.outcome_handle();
        sel.handle_input(&down_event());
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            SessionSelectorOutcome::Confirmed(p) => {
                // First row is 2025-05-10, second is 2025-05-09.
                assert_eq!(p.thread_id, "2025-05-09");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn typing_filters_the_list_and_enter_commits_top_match() {
        let catalog = sample_catalog();
        let mut sel = SessionSelectorComponent::new(identity_theme(), catalog, None, None);
        let outcome = sel.outcome_handle();
        // Type "stream" — only "debug the streaming protocol"
        // should remain.
        for c in "stream".chars() {
            sel.handle_input(&Key::char(c));
        }
        let body = sel.render(80).join("\n");
        assert!(body.contains("debug the streaming protocol"), "got: {body}");
        assert!(!body.contains("refactor"), "got: {body}");
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            SessionSelectorOutcome::Confirmed(p) => {
                assert_eq!(p.thread_id, "2025-05-09");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn initial_query_pre_fills_search_and_filters_immediately() {
        let catalog = sample_catalog();
        let mut sel = SessionSelectorComponent::new(
            identity_theme(),
            catalog,
            None,
            Some("refactor".to_string()),
        );
        let body = sel.render(80).join("\n");
        assert!(body.contains("refactor the agent loop"), "got: {body}");
        assert!(!body.contains("debug"), "got: {body}");
    }

    #[test]
    fn empty_catalog_renders_no_match_placeholder() {
        let mut sel = SessionSelectorComponent::new(identity_theme(), vec![], None, None);
        let body = sel.render(80).join("\n");
        // SelectList renders "No matching ..." when filtered
        // indices is empty.
        assert!(body.contains("No matching"), "got: {body}");
    }

    #[test]
    fn thread_with_no_user_message_shows_placeholder_in_primary() {
        let catalog = vec![make_preview("2025-05-11", None, 0, Duration::seconds(10))];
        let mut sel = SessionSelectorComponent::new(identity_theme(), catalog, None, None);
        let body = sel.render(80).join("\n");
        assert!(body.contains("(no user message yet)"), "got: {body}");
    }

    #[test]
    fn long_preview_text_is_truncated_with_ellipsis() {
        // Test the truncation helper directly so SelectList's
        // separate column-allocation truncation doesn't mask the
        // result.
        let very_long: String = "a ".repeat(200);
        let preview = make_preview("2025-05-11", Some(&very_long), 1, Duration::seconds(10));
        let primary = format_primary(&preview, false);
        assert!(
            primary.contains('…'),
            "expected an ellipsis in truncated primary: {primary:?}"
        );
        // And the truncation cap matches PREVIEW_MAX_CHARS.
        assert_eq!(primary.chars().count(), PREVIEW_MAX_CHARS);
    }

    #[test]
    fn format_age_uses_expected_buckets() {
        let now = Utc::now();
        assert_eq!(format_age(now, now - Duration::seconds(10)), "now");
        assert_eq!(format_age(now, now - Duration::minutes(3)), "3m");
        assert_eq!(format_age(now, now - Duration::hours(2)), "2h");
        assert_eq!(format_age(now, now - Duration::days(3)), "3d");
        assert_eq!(format_age(now, now - Duration::days(14)), "2w");
        assert_eq!(format_age(now, now - Duration::days(60)), "2mo");
        assert_eq!(format_age(now, now - Duration::days(800)), "2y");
    }
}
