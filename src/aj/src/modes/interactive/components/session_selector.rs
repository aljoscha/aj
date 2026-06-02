//! Session-selector overlay (`/sessions`).
//!
//! Pairs a [`aj_tui::components::text_input::TextInput`] for live
//! substring filtering with a
//! [`aj_tui::components::select_list::SelectList`] of
//! [`aj_session::SessionPreview`]s. The host opens this overlay from
//! `/sessions`; `Enter` commits the highlighted session, `Esc`
//! cancels.
//!
//! The previews are scanned on a blocking thread, not on the TUI event
//! loop: the overlay opens immediately (showing a loading indicator)
//! and the list fills in incrementally as the scan streams batches (one
//! per session file, newest-first) through an internal channel drained
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
//!
//! See `docs/aj-next-plan.md` Phase 1 §4 "Selectors and theming".

use std::sync::{Arc, Mutex};

use aj_session::SessionPreview;
use aj_tui::component::Component;
use aj_tui::components::select_list::{
    FilterMode, SelectItem, SelectList, SelectListLayout, SelectListTheme,
};
use aj_tui::components::text_input::TextInput;
use aj_tui::keybindings;
use aj_tui::keys::InputEvent;
use aj_tui::tui::RenderHandle;
use chrono::{DateTime, Utc};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

/// Cap on how much of the first user message is rendered in the
/// primary column. Keeps long pastes (a stack trace, a chunk of
/// code) from blowing past the overlay width while leaving room
/// for the broader `<count> msgs · created … · last …`
/// description triplet — the description column needs more width
/// than the previous design left it.
const PREVIEW_MAX_CHARS: usize = 60;

/// Outcome of a single overlay session.
///
/// `Confirmed(preview)` carries the chosen [`SessionPreview`]
/// (cloned so the host can open the matching log without
/// borrowing the selector's entries); `Cancelled` is the user
/// pressing `Esc`.
/// The host treats both as "close the overlay"; only the former
/// triggers the swap-session flow.
#[derive(Debug, Clone)]
pub enum SessionSelectorOutcome {
    Confirmed(SessionPreview),
    Cancelled,
}

/// Cheap-to-clone handle pointing at the same outcome slot the
/// overlay component writes into.
pub type OutcomeHandle = Arc<Mutex<Option<SessionSelectorOutcome>>>;

/// Result of the background scan, delivered to the live overlay. The
/// component drains these at the top of `render`, appending batches and
/// clearing the loading indicator on `Done`.
enum SessionLoad {
    /// A batch of previews, appended in arrival (newest-first) order.
    Batch(Vec<SessionPreview>),
    /// The scan finished; clears the loading indicator.
    Done,
}

/// The overlay's top-level component.
///
/// Owns the search input (`search`), the inner [`SelectList`]
/// (`list`), the previews accumulated from the background scan
/// (`entries`), and the outcome slot (`outcome`). The host keeps
/// another clone of `outcome` and polls it after every input event to
/// decide whether to close the overlay.
pub struct SessionSelectorComponent {
    /// Search box at the top of the overlay. Typing into it filters
    /// `list`; Enter is intercepted at the component level so it
    /// commits the highlighted list item.
    search: TextInput,
    /// Result list. Built empty and filled by appending streamed
    /// batches; filtered in place via [`SelectList::set_filter`] on
    /// each keystroke.
    list: SelectList,
    /// Previews accumulated from the background scan, in arrival
    /// (newest-first) order. The component clones the entry it emits on
    /// confirm; keeping the source of truth here avoids any chance of
    /// drift between filter and confirm.
    entries: Vec<SessionPreview>,
    /// Session id of the agent's currently-active log. Used to
    /// pre-select that row once it streams in and mark it `(current)`
    /// so a no-op confirm is obvious.
    current_session_id: Option<String>,
    /// Whether the current session's row still needs to be selected.
    /// Set when `current_session_id` is present; cleared once the row
    /// is found and selected, or as soon as the user navigates / filters
    /// (so streaming never yanks the cursor away from the user).
    select_current_pending: bool,
    /// Whether the background scan is still running. Drives the loading
    /// indicator shown while the list is empty.
    loading: bool,
    /// Inbound scan results, drained in `render` and `handle_input`.
    loads_rx: UnboundedReceiver<SessionLoad>,
    /// Shared outcome slot. The host clones this handle once at
    /// construction and polls it after every input event.
    outcome: OutcomeHandle,
    /// Theme used to build the inner [`SelectList`]. Stored so the list
    /// can be reconstructed (e.g. on resize) without the host having to
    /// pass it back in.
    theme: SelectListTheme,
    /// Maximum visible rows in the result list, sized by the host
    /// from the overlay's resolved height so a taller box shows more
    /// candidates at once.
    max_visible_rows: usize,
    /// `now` snapshot taken at construction time. Used to format
    /// each row's age (`5m`, `3h`, …) without re-reading the clock
    /// on every batch. The selector closes within seconds in
    /// practice; "Just now" rows stay "Just now" for the whole
    /// session.
    now: DateTime<Utc>,
}

impl SessionSelectorComponent {
    /// Build a fresh selector and kick off the preview scan on a
    /// blocking thread. The list starts empty (showing a loading
    /// indicator) and fills in as the scan streams batches.
    ///
    /// `current_session_id` is the agent's active session — used to
    /// pre-select the matching row and mark it `(current)`.
    /// `initial_query`, when set, pre-fills the search box so the
    /// overlay opens already filtered; the slash layer passes `None`,
    /// but the parameter is kept as a general capability. `theme` styles
    /// the underlying [`SelectList`]. `max_visible_rows` caps how many
    /// result rows the list shows at once. `scan` drives the streaming
    /// preview walk (see
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
        let mut search = TextInput::new("search: ");
        if let Some(q) = &initial_query {
            search.set_value(q);
        }
        search.set_focused(true);

        let mut list = SelectList::new(
            Vec::new(),
            max_visible_rows,
            theme.clone(),
            primary_column_layout(),
        );
        list.set_focused(true);
        if let Some(q) = &initial_query {
            list.set_filter(q);
        }

        let (loads_tx, loads_rx) = tokio::sync::mpsc::unbounded_channel();
        spawn_scan(scan, loads_tx, render_handle);

        Self {
            search,
            list,
            entries: Vec::new(),
            select_current_pending: current_session_id.is_some(),
            current_session_id,
            loading: true,
            loads_rx,
            outcome: Arc::new(Mutex::new(None)),
            theme,
            max_visible_rows,
            now: Utc::now(),
        }
    }

    /// Hand the host a clone of the outcome slot. After each input
    /// event the host calls `lock().take()` on this handle; on
    /// `Some(_)` it hides the overlay and applies the result.
    pub fn outcome_handle(&self) -> OutcomeHandle {
        Arc::clone(&self.outcome)
    }

    /// Apply scan results delivered since the last drain: append each
    /// batch to `entries` and stream its rows into the live list,
    /// clearing the loading flag on `Done`.
    ///
    /// New rows are coalesced into a single [`SelectList::extend_items`]
    /// call so a burst of batches in one frame costs one append (and,
    /// when a filter is active, one re-rank) rather than one per batch.
    /// After appending, if the current session's row is still pending
    /// selection and has now arrived, the highlight moves to it.
    fn drain_loads(&mut self) {
        let mut new_items: Vec<SelectItem> = Vec::new();
        while let Ok(load) = self.loads_rx.try_recv() {
            match load {
                SessionLoad::Batch(previews) => {
                    for preview in &previews {
                        new_items.push(self.build_item(preview));
                    }
                    self.entries.extend(previews);
                }
                SessionLoad::Done => self.loading = false,
            }
        }
        if !new_items.is_empty() {
            self.list.extend_items(new_items);
            self.try_select_current();
        }
    }

    /// Move the highlight to the current session's row if it's pending
    /// and present. A no-op once the user has taken over navigation
    /// (`select_current_pending` cleared) or once the row is selected.
    fn try_select_current(&mut self) {
        if !self.select_current_pending {
            return;
        }
        if let Some(id) = self.current_session_id.as_ref()
            && self.list.select_by_value(id)
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

    /// Commit the currently-highlighted list entry into the outcome
    /// slot. Looks the entry up in `entries` by its `session_id` to
    /// recover the full [`SessionPreview`].
    fn commit_selection(&self) {
        let Some(item) = self.list.selected_item().cloned() else {
            return;
        };
        let Some(info) = self
            .entries
            .iter()
            .find(|p| p.session_id == item.value)
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
/// when none is captured. Truncated to keep long pastes from
/// blowing past the overlay width.
fn format_primary(preview: &SessionPreview, is_current: bool) -> String {
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
        self.drain_loads();
        // Chrome (title + border) is provided by the surrounding
        // `OverlayWindow` at mount time; we render just the search
        // input stacked above the result list.
        let mut lines = Vec::with_capacity(self.max_visible_rows + 2);
        lines.extend(self.search.render(width));
        lines.push(String::new());
        if self.loading && self.entries.is_empty() {
            lines.push((self.theme.description)("Loading sessions…"));
        } else {
            lines.extend(self.list.render(width));
        }
        lines
    }

    fn handle_input(&mut self, event: &InputEvent) -> bool {
        self.drain_loads();
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

        // Navigation keys belong to the list. Once the user navigates,
        // stop chasing the current-session row on later batches.
        if kb.matches(event, "tui.select.up")
            || kb.matches(event, "tui.select.down")
            || kb.matches(event, "tui.select.pageUp")
            || kb.matches(event, "tui.select.pageDown")
        {
            drop(kb);
            self.select_current_pending = false;
            return self.list.handle_input(event);
        }

        // Everything else goes to the search box. Drop the
        // keybinding registry guard first so the filter below can
        // re-acquire it without contention.
        drop(kb);

        let before = self.search.value().to_string();
        let handled = self.search.handle_input(event);
        if handled && self.search.value() != before {
            // Filtering is a user-driven selection too: it resets the
            // list to the top match, so stop chasing the current row.
            self.select_current_pending = false;
            self.list.set_filter(self.search.value());
        }
        handled
    }

    fn set_focused(&mut self, focused: bool) {
        self.search.set_focused(focused);
        self.list.set_focused(focused);
    }

    fn set_available_height(&mut self, rows: usize) {
        // Chrome above the list: search input + blank separator + the
        // list's own scroll-info line.
        self.max_visible_rows = rows.saturating_sub(3).max(1);
        self.list.set_max_visible(self.max_visible_rows);
    }

    fn is_focused(&self) -> bool {
        self.search.is_focused()
    }
}

// ---------------------------------------------------------------------------
// Background scanning
// ---------------------------------------------------------------------------

/// Drive the streaming `scan` on a blocking thread, forwarding each
/// batch it emits to the overlay's channel and waking the TUI; a `Done`
/// marker follows so the overlay can drop its loading indicator. Outside
/// a Tokio runtime (unit tests) the scan runs inline so results are
/// delivered synchronously.
fn spawn_scan(
    scan: impl FnOnce(&mut dyn FnMut(Vec<SessionPreview>)) + Send + 'static,
    tx: UnboundedSender<SessionLoad>,
    render_handle: RenderHandle,
) {
    let run = move || {
        let mut emit = |previews: Vec<SessionPreview>| {
            if previews.is_empty() {
                return;
            }
            let _ = tx.send(SessionLoad::Batch(previews));
            render_handle.request_render();
        };
        scan(&mut emit);
        let _ = tx.send(SessionLoad::Done);
        render_handle.request_render();
    };
    match tokio::runtime::Handle::try_current() {
        Ok(_) => {
            tokio::task::spawn_blocking(run);
        }
        Err(_) => run(),
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
    /// Tests run outside a Tokio runtime, so [`spawn_scan`] executes
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
        let body = sel.render(200).join("\n");
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
        // it verbatim.
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            SessionSelectorOutcome::Confirmed(p) => {
                assert_eq!(p.session_id, "2025-05-10");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn esc_emits_cancelled_outcome() {
        let mut sel = selector(sample_catalog(), None, None);
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
        // No current session → first row pre-selected.
        let mut sel = selector(sample_catalog(), None, None);
        let outcome = sel.outcome_handle();
        sel.handle_input(&down_event());
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            SessionSelectorOutcome::Confirmed(p) => {
                // First row is 2025-05-10, second is 2025-05-09.
                assert_eq!(p.session_id, "2025-05-09");
            }
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
        let body = sel.render(80).join("\n");
        assert!(body.contains("debug the streaming protocol"), "got: {body}");
        assert!(!body.contains("refactor"), "got: {body}");
        sel.handle_input(&enter_event());
        let result = outcome.lock().unwrap().take().expect("outcome was set");
        match result {
            SessionSelectorOutcome::Confirmed(p) => {
                assert_eq!(p.session_id, "2025-05-09");
            }
            other => panic!("expected Confirmed, got {other:?}"),
        }
    }

    #[test]
    fn initial_query_pre_fills_search_and_filters_immediately() {
        let mut sel = selector(sample_catalog(), None, Some("refactor"));
        let body = sel.render(80).join("\n");
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
        let body = sel.render(80).join("\n");
        assert!(
            !body.contains("debug the streaming protocol"),
            "got: {body}"
        );
        assert!(body.contains("No sessions in this project"), "got: {body}");
    }

    #[test]
    fn empty_catalog_renders_no_match_placeholder() {
        let mut sel = selector(vec![], None, None);
        let body = sel.render(80).join("\n");
        // With no sessions the list renders its configured empty
        // message once the scan completes.
        assert!(body.contains("No sessions in this project"), "got: {body}");
    }

    #[test]
    fn session_with_no_user_message_shows_placeholder_in_primary() {
        let catalog = vec![make_preview("2025-05-11", None, 0, Duration::seconds(10))];
        let mut sel = selector(catalog, None, None);
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
        let body = sel.render(200).join("\n");
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
        let body = sel.render(200).join("\n");
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
        match outcome.lock().unwrap().take().expect("outcome was set") {
            SessionSelectorOutcome::Confirmed(p) => assert_eq!(p.session_id, "2025-05-09"),
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
}
