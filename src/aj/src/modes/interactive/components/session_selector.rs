//! Session-selector overlay (`/resume`).
//!
//! Pairs a search box with a
//! [`aj_tui::components::select_list::SelectList`] of
//! [`aj_session::SessionPreview`]s. The host opens this overlay from
//! `/resume`; `Enter` commits the highlighted session, `Esc` cancels.
//!
//! The previews are scanned on a blocking thread, not on the TUI event
//! loop: the overlay opens immediately (showing a loading indicator)
//! and the list fills in incrementally as the scan streams batches (one
//! per session file, newest-first) through a [`StreamingScan`] drained
//! at the top of `render`. Like the command palette, the list is built
//! once and filtered via [`SelectList::set_filter`] on each keystroke
//! rather than rebuilt; arriving batches are appended in place with
//! [`SelectList::extend_items`].
//!
//! The currently-active session is pre-selected once its row streams in
//! and tagged `(current)` so a no-op confirm is visually obvious — the
//! user can verify "yes, I'm staying on this one" without scanning the
//! file id. The pre-selection yields the moment the user starts
//! navigating or filtering so streaming results never yank the cursor.

use std::sync::Arc;

use aj_session::SessionPreview;
use aj_tui::ansi::{strip_ansi, truncate_to_width};
use aj_tui::component::Component;
use aj_tui::components::filterable_select::FilterableSelect;
use aj_tui::components::select_list::{
    FilterMode, SelectItem, SelectList, SelectListLayout, SelectListTheme,
};
use aj_tui::keys::InputEvent;
use aj_tui::tui::RenderHandle;
use chrono::{DateTime, Utc};

use crate::modes::interactive::components::outcome::OutcomeSlot;
use crate::modes::interactive::components::streaming_scan::StreamingScan;

/// Cap on how much of the first user message is rendered in the
/// primary column. Keeps long pastes (a stack trace, a chunk of
/// code) from blowing past the overlay width while leaving room
/// for the broader `<count> msgs · created … · last …`
/// description triplet — the description column needs more width
/// than the previous design left it.
const PREVIEW_MAX_CHARS: usize = 60;

/// Outcome of a single overlay session.
///
/// `Confirmed(session_id)` carries the chosen session's id (the host
/// reopens the matching log); `Cancelled` is the user pressing `Esc`.
/// The host treats both as "close the overlay"; only the former triggers
/// the swap-session flow.
#[derive(Debug, Clone)]
pub enum SessionSelectorOutcome {
    Confirmed(String),
    Cancelled,
}

/// Cheap-to-clone handle pointing at the same outcome slot the
/// overlay component writes into.
pub type OutcomeHandle = OutcomeSlot<SessionSelectorOutcome>;

/// The overlay's top-level component: a [`FilterableSelect`] fed by a
/// background [`StreamingScan`].
pub struct SessionSelectorComponent {
    inner: FilterableSelect,
    /// Background preview scan, drained at the top of `render` /
    /// `handle_input`.
    scan: StreamingScan<SessionPreview>,
    /// Session id of the agent's currently-active log. Used to
    /// pre-select that row once it streams in and mark it `(current)`.
    current_session_id: Option<String>,
    /// Whether the current session's row still needs to be selected.
    /// Cleared once the row is found, or as soon as the user navigates /
    /// filters (so streaming never yanks the cursor away from the user).
    select_current_pending: bool,
    /// `now` snapshot taken at construction time, used to format each
    /// row's age without re-reading the clock per batch.
    now: DateTime<Utc>,
    outcome: OutcomeHandle,
}

impl SessionSelectorComponent {
    /// Build a fresh selector and kick off the preview scan on a
    /// blocking thread. The list starts empty (showing a loading
    /// indicator) and fills in as the scan streams batches.
    ///
    /// `current_session_id` is the agent's active session — used to
    /// pre-select the matching row and mark it `(current)`.
    /// `initial_query`, when set, pre-fills the search box so the
    /// overlay opens already filtered; the host passes `None`, but the
    /// parameter is kept as a general capability. `theme` styles the
    /// underlying [`SelectList`]. `max_visible_rows` caps how many result
    /// rows the list shows at once. `scan` drives the streaming preview
    /// walk (see
    /// [`aj_session::ConversationPersistence::list_session_previews_streaming`]),
    /// emitting one batch per session file in newest-first order.
    pub fn new(
        theme: SelectListTheme,
        current_session_id: Option<String>,
        initial_query: Option<String>,
        max_visible_rows: usize,
        render_handle: RenderHandle,
        scan: impl FnOnce(&mut dyn FnMut(Vec<SessionPreview>)) + Send + 'static,
    ) -> Self {
        let status_style = Arc::clone(&theme.description);
        let list = SelectList::new(Vec::new(), max_visible_rows, theme, primary_column_layout());
        let mut inner = FilterableSelect::new("search: ", list, status_style);
        inner.set_loading_message("Loading sessions…");
        inner.set_loading(true);

        let outcome = OutcomeHandle::new();
        let confirm = outcome.clone();
        inner.on_select = Some(Box::new(move |item| {
            confirm.set(SessionSelectorOutcome::Confirmed(item.value.clone()));
        }));
        let cancel = outcome.clone();
        inner.on_cancel = Some(Box::new(move || {
            cancel.set(SessionSelectorOutcome::Cancelled)
        }));

        if let Some(q) = &initial_query {
            inner.set_query(q);
        }

        let select_current_pending = current_session_id.is_some();
        Self {
            inner,
            scan: StreamingScan::spawn(scan, render_handle),
            current_session_id,
            select_current_pending,
            now: Utc::now(),
            outcome,
        }
    }

    /// Hand the host a clone of the outcome slot. After each input
    /// event the host calls `take()` on this handle; on `Some(_)` it
    /// hides the overlay and applies the result.
    pub fn outcome_handle(&self) -> OutcomeHandle {
        self.outcome.clone()
    }

    /// Apply scan results delivered since the last drain: stream the new
    /// rows into the live list (coalesced into one
    /// [`SelectList::extend_items`]), chase the current row if still
    /// pending, and sync the loading indicator.
    fn drain(&mut self) {
        let new = self.scan.drain();
        if !new.is_empty() {
            let items: Vec<SelectItem> = new.iter().map(|p| self.build_item(p)).collect();
            self.inner.list_mut().extend_items(items);
            self.try_select_current();
        }
        self.inner.set_loading(self.scan.is_loading());
    }

    /// Move the highlight to the current session's row if it's pending
    /// and present. A no-op once the user has taken over navigation
    /// (`select_current_pending` cleared) or once the row is selected.
    fn try_select_current(&mut self) {
        if !self.select_current_pending {
            return;
        }
        // Clone the id so the immutable borrow of `current_session_id`
        // doesn't overlap the mutable borrow of `inner`.
        if let Some(id) = self.current_session_id.clone()
            && self.inner.list_mut().select_by_value(&id)
        {
            self.select_current_pending = false;
        }
    }

    /// Build one [`SelectItem`] for a preview. The `(current)` marker is
    /// baked into the label at build time so it survives filtering; the
    /// session id is the row value and the searchable text covers both
    /// the first user message and the id.
    fn build_item(&self, preview: &SessionPreview) -> SelectItem {
        let is_current = self
            .current_session_id
            .as_ref()
            .is_some_and(|id| id == &preview.session_id);
        let primary = format_primary(preview, is_current);
        let secondary = format_secondary(preview, self.now);
        SelectItem::new(&preview.session_id, &primary)
            .with_description(&secondary)
            .with_filter_key(&haystack_for(preview))
    }
}

/// Searchable text for `preview`. Joins the first user message
/// (when present) and the session id so typing either a substring
/// of the prompt or part of the timestamp finds the row.
fn haystack_for(preview: &SessionPreview) -> String {
    let first = preview.first_user_message.as_deref().unwrap_or("");
    format!("{} {}", first, preview.session_id)
}

/// Build the layout used for the inner [`SelectList`].
///
/// The default `SelectListLayout` allocates only 32 chars to the
/// primary column, which truncates our `<preview> (current)` rows.
/// Lift the cap to leave room for both the preview text and the
/// `(current)` suffix; the description column shrinks accordingly.
///
/// Filtering uses [`FilterMode::SubstringAllTokens`]: the searchable
/// text is the whole first user message plus the session id, which can
/// be long, so fuzzy subsequence matching is too permissive (a
/// multi-word query subsequence-matches almost any prompt). Each
/// whitespace-separated query token must instead appear as a
/// case-insensitive substring, and matches keep their newest-first
/// order.
fn primary_column_layout() -> SelectListLayout {
    SelectListLayout {
        // PREVIEW_MAX_CHARS for the preview text + 10 chars for
        // " (current)" + 2-char inter-column gap that
        // `SelectList` accounts for inside this width.
        max_primary_column_width: Some(PREVIEW_MAX_CHARS + 12),
        wrap_selection: false,
        filter_mode: FilterMode::SubstringAllTokens,
        empty_message: "No sessions in this project".to_string(),
        ..SelectListLayout::default()
    }
}

/// Build the primary (left) column for one row. The first user
/// message is the most recognisable handle; session id falls back
/// when none is captured.
///
/// Truncation goes through the display-width authority so wide/zero-width
/// glyphs are measured in cells, not chars. We then strip ANSI: the
/// truncator emits a reset sequence when it cuts, and `SelectList`
/// re-styles this label for the selected row, where an embedded reset
/// would bleed the selection highlight. Stripping also neutralizes any
/// escape codes in a pasted prompt, which a picker row shouldn't render.
fn format_primary(preview: &SessionPreview, is_current: bool) -> String {
    let raw = preview
        .first_user_message
        .as_deref()
        .unwrap_or("(no user message yet)");
    let one_line = raw.lines().next().unwrap_or(raw);
    let truncated = strip_ansi(&truncate_to_width(one_line, PREVIEW_MAX_CHARS, "…", false));
    if is_current {
        format!("{truncated} (current)")
    } else {
        truncated
    }
}

/// Build the secondary (right / description) column for one row.
/// Carries session metadata as a triplet: message count, creation
/// date (adaptive absolute), and time since the last message
/// (coarse buckets relative to `now`). The session id itself is
/// omitted — it's already the row's unique value and would
/// dominate the column width without adding much.
///
/// Example: `42 msgs · created May 8 · last 5m`.
///
/// If a too-narrow terminal can't fit the full triplet,
/// [`SelectList`]'s existing description-end truncation kicks in
/// — we don't build a custom collapse strategy.
fn format_secondary(preview: &SessionPreview, now: DateTime<Utc>) -> String {
    let count = preview.message_count;
    let msg_word = if count == 1 { "msg" } else { "msgs" };
    let created = format_created(now, preview.created_at);
    let last = format_age(now, preview.last_message_at);
    format!("{count} {msg_word} · created {created} · last {last}")
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

/// Render `created` as an adaptive absolute date relative to `now`.
///
/// - **Same calendar day** as `now`: clock-only (`14:22`). The
///   surrounding `last <age>` field already captures recency
///   coarsely, so the absolute clock time is the value-add for
///   sessions created earlier today.
/// - **Same calendar year** as `now`: month + day (`May 8`). Year
///   is implied; trimming it keeps the description tight.
/// - **Older**: month + day + year (`May 8 2024`). The year
///   matters once we cross the calendar boundary.
///
/// Both arguments are UTC `DateTime`s; the comparison is therefore
/// UTC-local rather than wall-clock-local. That's a deliberate
/// trade-off: the rest of the selector renders ages in UTC too
/// (the `session_id` mint format is `%Y-%m-%d-%H-%M-%S-%3f` UTC),
/// and switching one cell to wall-clock would make the row
/// internally inconsistent. A future per-user locale toggle could
/// flip everything together.
fn format_created(now: DateTime<Utc>, created: DateTime<Utc>) -> String {
    use chrono::Datelike;
    let same_day = now.date_naive() == created.date_naive();
    let same_year = now.year() == created.year();
    if same_day {
        created.format("%H:%M").to_string()
    } else if same_year {
        created.format("%b %-d").to_string()
    } else {
        created.format("%b %-d %Y").to_string()
    }
}

impl Component for SessionSelectorComponent {
    aj_tui::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<aj_tui::Line> {
        self.drain();
        self.inner.render(width)
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        self.drain();
        // A user-driven navigation or filter yields the current-row chase
        // so streaming batches never yank the cursor. We treat any
        // navigation key as intent (even one that doesn't move the
        // highlight, e.g. Up at the top row) plus any change to the query.
        let query_before = self.inner.query().to_string();
        let navigated = FilterableSelect::is_navigation_key(event);
        let handled = self.inner.handle_input(event);
        if navigated || self.inner.query() != query_before {
            self.select_current_pending = false;
        }
        handled
    }

    fn set_focused(&mut self, focused: bool) {
        self.inner.set_focused(focused);
    }

    fn set_available_height(&mut self, rows: usize) {
        self.inner.set_available_height(rows);
    }

    fn is_focused(&self) -> bool {
        self.inner.is_focused()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_tui::components::select_list::SelectListTheme;
    use aj_tui::keys::{InputEvent, Key};
    use chrono::Duration;

    use super::*;

    /// Visible-row cap used across the unit tests. The host derives
    /// this from the overlay height at runtime; the tests just need a
    /// fixed, generous value.
    const TEST_MAX_VISIBLE_ROWS: usize = 14;

    /// Identity theme for tests — passes every closure through
    /// verbatim so renders show structural text rather than ANSI.
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

    fn make_preview(
        session_id: &str,
        first_user: Option<&str>,
        message_count: usize,
        age: Duration,
    ) -> SessionPreview {
        let now = Utc::now();
        // Default test policy: `last_message_at` mirrors `age` (the
        // recency we care about), `created_at` is treated as roughly
        // the same instant — most tests don't care about the
        // distinction and the few that do override both fields
        // directly after constructing the preview.
        let last = now - age;
        SessionPreview {
            session_id: session_id.to_string(),
            modified: last,
            created_at: last,
            last_message_at: last,
            size_bytes: 1024,
            message_count,
            first_user_message: first_user.map(|s| s.to_string()),
        }
    }

    fn sample_catalog() -> Vec<SessionPreview> {
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

    /// Build a selector whose scan emits `catalog` as a single batch.
    /// Tests run outside a Tokio runtime, so [`StreamingScan`] executes
    /// the scan inline; the batch is delivered into the channel before
    /// `new` returns and drained on the first `render` / `handle_input`.
    fn selector(
        catalog: Vec<SessionPreview>,
        current: Option<&str>,
        initial_query: Option<&str>,
    ) -> SessionSelectorComponent {
        SessionSelectorComponent::new(
            identity_theme(),
            current.map(|s| s.to_string()),
            initial_query.map(|s| s.to_string()),
            TEST_MAX_VISIBLE_ROWS,
            RenderHandle::detached(),
            move |emit| emit(catalog),
        )
    }

    #[test]
    fn highlights_current_session_on_open() {
        let mut sel = selector(sample_catalog(), Some("2025-05-09"), None);
        // Render wide enough that SelectList's primary column
        // doesn't truncate the `(current)` suffix off the label.
        let body = sel
            .render(200)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // The middle row should be marked "(current)".
        assert!(
            body.contains("debug the streaming protocol (current)"),
            "got: {body}"
        );
    }

    #[test]
    fn enter_commits_highlighted_entry() {
        let mut sel = selector(sample_catalog(), Some("2025-05-10"), None);
        let outcome = sel.outcome_handle();
        // 2025-05-10 is pre-selected (current); Enter should commit
        // its id verbatim.
        sel.handle_input(&enter_event());
        match outcome.take().expect("outcome was set") {
            SessionSelectorOutcome::Confirmed(id) => assert_eq!(id, "2025-05-10"),
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn esc_emits_cancelled_outcome() {
        let mut sel = selector(sample_catalog(), None, None);
        let outcome = sel.outcome_handle();
        sel.handle_input(&escape_event());
        assert!(
            matches!(
                outcome.take().expect("outcome was set"),
                SessionSelectorOutcome::Cancelled
            ),
            "expected Cancelled"
        );
    }

    #[test]
    fn down_arrow_moves_to_next_row_then_enter_confirms_it() {
        // No current session → first row pre-selected.
        let mut sel = selector(sample_catalog(), None, None);
        let outcome = sel.outcome_handle();
        sel.handle_input(&down_event());
        sel.handle_input(&enter_event());
        match outcome.take().expect("outcome was set") {
            // First row is 2025-05-10, second is 2025-05-09.
            SessionSelectorOutcome::Confirmed(id) => assert_eq!(id, "2025-05-09"),
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn typing_filters_the_list_and_enter_commits_top_match() {
        let mut sel = selector(sample_catalog(), None, None);
        let outcome = sel.outcome_handle();
        // Type "stream" — only "debug the streaming protocol"
        // should remain.
        for c in "stream".chars() {
            sel.handle_input(&Key::char(c));
        }
        let body = sel
            .render(80)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("debug the streaming protocol"), "got: {body}");
        assert!(!body.contains("refactor"), "got: {body}");
        sel.handle_input(&enter_event());
        match outcome.take().expect("outcome was set") {
            SessionSelectorOutcome::Confirmed(id) => assert_eq!(id, "2025-05-09"),
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn initial_query_pre_fills_search_and_filters_immediately() {
        let mut sel = selector(sample_catalog(), None, Some("refactor"));
        let body = sel
            .render(80)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("refactor the agent loop"), "got: {body}");
        assert!(!body.contains("debug"), "got: {body}");
    }

    #[test]
    fn filter_is_substring_not_fuzzy_subsequence() {
        // "dbg" is a subsequence of "debug" (so fuzzy would match) but
        // not a substring, so substring filtering must exclude it.
        let mut sel = selector(sample_catalog(), None, None);
        for c in "dbg".chars() {
            sel.handle_input(&Key::char(c));
        }
        let body = sel
            .render(80)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !body.contains("debug the streaming protocol"),
            "got: {body}"
        );
        assert!(body.contains("No sessions in this project"), "got: {body}");
    }

    #[test]
    fn empty_catalog_renders_no_match_placeholder() {
        let mut sel = selector(vec![], None, None);
        let body = sel
            .render(80)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        // With no sessions the list renders its configured empty
        // message once the scan completes.
        assert!(body.contains("No sessions in this project"), "got: {body}");
    }

    #[test]
    fn session_with_no_user_message_shows_placeholder_in_primary() {
        let catalog = vec![make_preview("2025-05-11", None, 0, Duration::seconds(10))];
        let mut sel = selector(catalog, None, None);
        let body = sel
            .render(80)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
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
        // The cap is a *display-width* budget (the `ansi` authority),
        // not a char count: the truncated preview occupies exactly
        // PREVIEW_MAX_CHARS columns including the ellipsis.
        assert_eq!(
            aj_tui::ansi::visible_width(&primary),
            PREVIEW_MAX_CHARS,
            "got: {primary:?}"
        );
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

    #[test]
    fn format_created_uses_clock_for_same_day() {
        // A timestamp earlier the same calendar day should render
        // as `HH:MM` only.
        let now = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_opt(20, 0, 0)
            .unwrap()
            .and_utc();
        let earlier = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_opt(14, 22, 0)
            .unwrap()
            .and_utc();
        assert_eq!(format_created(now, earlier), "14:22");
    }

    #[test]
    fn format_created_uses_month_day_for_same_year() {
        // Different calendar day, same year → `Mon D`.
        let now = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_opt(20, 0, 0)
            .unwrap()
            .and_utc();
        let earlier = chrono::NaiveDate::from_ymd_opt(2025, 5, 8)
            .unwrap()
            .and_hms_opt(14, 22, 0)
            .unwrap()
            .and_utc();
        assert_eq!(format_created(now, earlier), "May 8");
    }

    #[test]
    fn format_created_uses_year_for_older_sessions() {
        // Different calendar year → `Mon D YYYY`.
        let now = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_opt(20, 0, 0)
            .unwrap()
            .and_utc();
        let earlier = chrono::NaiveDate::from_ymd_opt(2024, 5, 8)
            .unwrap()
            .and_hms_opt(14, 22, 0)
            .unwrap()
            .and_utc();
        assert_eq!(format_created(now, earlier), "May 8 2024");
    }

    #[test]
    fn description_carries_msg_count_created_and_last() {
        // The description should encode all three fields in the
        // documented order: `<count> msgs · created <D> · last <age>`.
        let now = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_opt(20, 0, 0)
            .unwrap()
            .and_utc();
        let p = SessionPreview {
            session_id: "2025-05-11-13-22-00-000".to_string(),
            // `last_message_at` and `created_at` separated by hours
            // so the rendered fields are visually distinct.
            modified: now - Duration::hours(2),
            created_at: chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
                .unwrap()
                .and_hms_opt(13, 22, 0)
                .unwrap()
                .and_utc(),
            last_message_at: now - Duration::hours(2),
            size_bytes: 0,
            message_count: 42,
            first_user_message: Some("refactor".into()),
        };
        let secondary = format_secondary(&p, now);
        assert_eq!(secondary, "42 msgs · created 13:22 · last 2h");
    }

    #[test]
    fn description_singular_msg_word_for_one_message() {
        // The singular grammar (`1 msg`) is preserved.
        let now = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_opt(20, 0, 0)
            .unwrap()
            .and_utc();
        let p = SessionPreview {
            session_id: "2025-05-11-13-22-00-000".into(),
            modified: now,
            created_at: chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
                .unwrap()
                .and_hms_opt(13, 22, 0)
                .unwrap()
                .and_utc(),
            last_message_at: now,
            size_bytes: 0,
            message_count: 1,
            first_user_message: None,
        };
        let s = format_secondary(&p, now);
        assert!(s.starts_with("1 msg ·"), "got: {s:?}");
    }

    #[test]
    fn description_uses_last_message_at_not_modified_for_age() {
        // A preview whose file mtime is recent but whose final
        // message timestamp is hours older should still render
        // `last 3h` rather than `last now`.
        let now = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_opt(20, 0, 0)
            .unwrap()
            .and_utc();
        let p = SessionPreview {
            session_id: "2025-05-11-13-22-00-000".into(),
            // File was touched seconds ago (e.g. a vacuum / rename
            // / fsync), but the last actual message landed 3h ago.
            modified: now - Duration::seconds(5),
            created_at: chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
                .unwrap()
                .and_hms_opt(13, 22, 0)
                .unwrap()
                .and_utc(),
            last_message_at: now - Duration::hours(3),
            size_bytes: 0,
            message_count: 10,
            first_user_message: None,
        };
        let s = format_secondary(&p, now);
        // The `last <age>` cell drives off `last_message_at`, so we
        // get `last 3h` even though the file was touched moments
        // ago.
        assert!(s.ends_with("· last 3h"), "got: {s:?}");
    }

    #[test]
    fn pre_selected_current_session_stays_visible_at_max_visible_rows_bump() {
        // With a generous row cap, a catalog of 12 sessions and the
        // current session in the middle of the list should produce a
        // render that still includes the `(current)` marker — i.e.
        // no off-screen scrolling required.
        let mut catalog = Vec::new();
        for i in 0i64..12 {
            catalog.push(make_preview(
                &format!("2025-05-{:02}", 11 - i),
                Some(&format!("session {i}")),
                // Test-only count: small numeric, so a lossless
                // cast through a typed temporary keeps the strict
                // `clippy::as-conversions` lint satisfied.
                usize::try_from(i).expect("non-negative i fits in usize"),
                Duration::minutes(i * 5 + 1),
            ));
        }
        // Pick the middle row as current. The pre-selection should
        // land in the visible window without any scroll.
        let current = catalog[6].session_id.clone();
        let mut sel = selector(catalog, Some(&current), None);
        // Render wide so column truncation can't strip "(current)".
        let body = sel
            .render(200)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            body.contains("session 6 (current)"),
            "expected the current row to be visible: {body}"
        );
    }

    #[test]
    fn streamed_batches_accumulate_in_arrival_order() {
        // Each emit is a separate batch; the list appends them rather
        // than replacing, preserving arrival (newest-first) order.
        let mut sel = SessionSelectorComponent::new(
            identity_theme(),
            None,
            None,
            TEST_MAX_VISIBLE_ROWS,
            RenderHandle::detached(),
            |emit| {
                emit(vec![make_preview(
                    "2025-05-10",
                    Some("newest session"),
                    1,
                    Duration::minutes(1),
                )]);
                emit(vec![make_preview(
                    "2025-05-09",
                    Some("older session"),
                    1,
                    Duration::hours(1),
                )]);
            },
        );
        let body = sel
            .render(200)
            .iter()
            .map(|l| l.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let newest = body.find("newest session").expect("newest shown");
        let older = body.find("older session").expect("older shown");
        assert!(newest < older, "expected newest before older, got: {body}");
    }

    #[test]
    fn current_session_selected_regardless_of_arrival_position() {
        // The current row arrives in the second batch; the highlight
        // should land on it wherever it appears (the user hasn't
        // navigated), not default to the first row.
        let mut sel = SessionSelectorComponent::new(
            identity_theme(),
            Some("2025-05-09".to_string()),
            None,
            TEST_MAX_VISIBLE_ROWS,
            RenderHandle::detached(),
            |emit| {
                emit(vec![make_preview(
                    "2025-05-10",
                    Some("first batch"),
                    1,
                    Duration::minutes(1),
                )]);
                emit(vec![make_preview(
                    "2025-05-09",
                    Some("current session"),
                    1,
                    Duration::hours(1),
                )]);
            },
        );
        let outcome = sel.outcome_handle();
        // Drain + confirm without navigating: the committed entry is the
        // current session, even though it's not the first row.
        sel.handle_input(&enter_event());
        match outcome.take().expect("outcome was set") {
            SessionSelectorOutcome::Confirmed(id) => assert_eq!(id, "2025-05-09"),
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn navigation_clears_pending_current_selection() {
        // Navigating clears the pending flag so a later batch carrying
        // the current row can't yank the cursor back. Use a current id
        // absent from the catalog so the drain itself can't clear the
        // flag — only the Down keypress should.
        let mut sel = selector(sample_catalog(), Some("not-in-catalog"), None);
        assert!(sel.select_current_pending);
        sel.handle_input(&down_event());
        assert!(
            !sel.select_current_pending,
            "navigation should stop the selector chasing the current row"
        );
    }

    #[test]
    fn boundary_navigation_keypress_still_clears_pending() {
        // Up at the top row doesn't move the highlight, but it's still a
        // deliberate navigation: the chase must yield so a current row
        // arriving in a late batch can't yank the cursor down. (Lists
        // here don't wrap, so this is a genuine no-op move.)
        let mut sel = selector(sample_catalog(), Some("not-in-catalog"), None);
        assert!(sel.select_current_pending);
        sel.handle_input(&Key::up());
        assert!(
            !sel.select_current_pending,
            "a no-op boundary navigation should still stop the chase"
        );
    }
}
