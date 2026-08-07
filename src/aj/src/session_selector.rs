//! The session-selector overlay: resume a previous session in place.
//!
//! A [`FilterableSelect`] over the project's session previews. Confirming
//! a row parks a [`SessionRequest::Resume`] for the host, which tears the
//! current session down and rebuilds onto the chosen one. Choosing the
//! session already active is a no-op close, and Esc cancels.
//!
//! The preview scan is off the drive loop: the overlay opens showing a
//! loading placeholder and the host streams rows in as the scan (run on a
//! blocking thread over [`ConversationPersistence`](aj_session::ConversationPersistence))
//! emits per-file batches, so the list fills progressively rather than
//! blocking on the whole walk. Rows are newest-first, the active session
//! pre-selected and tagged `(current)`.
//!
//! A row's filter key is `"{first_user_message} {session_id}"` so typing
//! either the prompt or the id finds it, matching aj's selector. The
//! confirmed value is the session id, recovered through a shared
//! filter-key -> id map (the same indirection the command palette uses for
//! its actions), since the widget hands the confirm callback only the row's
//! filter key.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use aj_app::session::SessionRequest;
use aj_session::SessionPreview;
use chrono::{DateTime, Datelike, Utc};
use vaxis::vxfw::{FilterableSelect, SelectItem, to_widget_ref};

use crate::interactive::OverlayHandles;
use crate::overlay::{OverlayPlacement, close_all, close_key_label, close_top, confirm_key_label};
use crate::settings_ui::push_window;

/// How much of the first user message a row's primary column shows before
/// truncating with an ellipsis.
const PREVIEW_MAX_CHARS: usize = 60;

/// A parked request for the host to scan session previews and fill the
/// selector's list. The select handle is `!Send`, so it stays on the host
/// side; the spawned scan produces only the (Send) previews.
pub(crate) struct SessionScan {
    pub(crate) select: Rc<RefCell<FilterableSelect>>,
    /// The active session id, for the `(current)` tag, the pre-selection,
    /// and the no-op-on-current confirm check.
    current: String,
    /// filter_key -> session_id, filled by [`extend_session_scan`] and read
    /// by the confirm callback (which sees only the row's filter key).
    ids: Rc<RefCell<HashMap<String, String>>>,
}

impl SessionScan {
    /// The filter key of the currently highlighted row, or `None` while the
    /// filtered set is empty. The host compares this across batches to tell
    /// whether the user has moved the selection off where the chase last
    /// parked it, so a late batch can stop chasing rather than yank the
    /// cursor.
    pub(crate) fn selected_filter_key(&self) -> Option<String> {
        self.select.borrow().selected().map(|item| item.filter_key)
    }
}

/// The single loading placeholder shown until the scan lands. Its empty
/// filter key is absent from the id map, so a confirm on it is inert.
fn loading_items() -> Vec<SelectItem> {
    vec![SelectItem::new("Loading\u{2026}", "")]
}

/// Open the session selector, showing a loading placeholder and parking a
/// scan for the host in `handles.session_scan`. A confirmed non-current row
/// lands a [`SessionRequest::Resume`] in `handles.session_request`; the
/// current row (or Esc) just closes. Does not move focus: the caller posts
/// the refocus event.
///
/// A switch is never refused for being busy. The session left behind stays
/// attached and keeps folding, so its turn finishes unwatched.
pub(crate) fn open_session_selector(handles: &OverlayHandles, current: String) {
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        loading_items(),
        handles.chrome.select.clone(),
    )));
    let focus = select.borrow().focus_target();
    let ids: Rc<RefCell<HashMap<String, String>>> = Rc::new(RefCell::new(HashMap::new()));
    {
        let mut sel = select.borrow_mut();
        // A project can hold many sessions, so show the vertical scroll bar.
        sel.set_show_scrollbar(true);
        let ids_c = Rc::clone(&ids);
        let current_c = current.clone();
        let request_c = Rc::clone(&handles.session_request);
        let stack_c = Rc::clone(&handles.stack);
        let editor_c = Rc::clone(&handles.editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            // The loading placeholder (and any row not yet in the map) has
            // no session to resume, so leave the overlay open.
            let Some(session_id) = ids_c.borrow().get(&item.filter_key).cloned() else {
                return;
            };
            // Choosing the active session changes nothing, so just close.
            if session_id == current_c {
                close_all(&stack_c, ctx, &editor_c);
                return;
            }
            *request_c.borrow_mut() = Some(SessionRequest::Resume(session_id));
            // A confirmed pick is terminal: tear the whole stack down
            // (palette included) back to the transcript. Cancel below uses
            // `close_top`, which returns to the palette underneath.
            close_all(&stack_c, ctx, &editor_c);
        }));
        let stack_cancel = Rc::clone(&handles.stack);
        let editor_cancel = Rc::clone(&handles.editor);
        sel.on_cancel = Some(Box::new(move |ctx| {
            close_top(&stack_cancel, ctx, &editor_cancel)
        }));
    }
    push_window(
        &handles.stack,
        &handles.chrome,
        "Resume session",
        subtitle(),
        to_widget_ref(Rc::clone(&select)),
        focus,
        OverlayPlacement::Large,
    );
    *handles.session_scan.borrow_mut() = Some(SessionScan {
        select,
        current,
        ids,
    });
}

/// Append a streamed batch of previews to the selector's list: build one
/// row per preview (newest-first, as scanned), record the filter-key -> id
/// map for confirm, and optionally pre-select the active session's row.
///
/// `first` replaces the loading placeholder with this batch (the initial
/// fill), later batches append in place keeping the cursor and scroll.
/// `chase_current` asks to pre-select the active session's row this batch.
/// Returns whether that row was found and selected, so the host can stop
/// chasing once it lands. Deciding when the chase should give up (e.g. the
/// user has started navigating) is the host's call, not this function's.
pub(crate) fn extend_session_scan(
    scan: &SessionScan,
    previews: &[SessionPreview],
    now: DateTime<Utc>,
    first: bool,
    chase_current: bool,
) -> bool {
    let items: Vec<SelectItem> = {
        let mut ids = scan.ids.borrow_mut();
        previews
            .iter()
            .map(|preview| {
                let is_current = preview.session_id == scan.current;
                let item = build_item(preview, is_current, now);
                ids.insert(item.filter_key.clone(), preview.session_id.clone());
                item
            })
            .collect()
        // Drop the map borrow before touching the select: the confirm
        // callback (fired from the widget's own dispatch) reads the map.
    };
    if first {
        scan.select.borrow().set_items(items);
    } else {
        scan.select.borrow().extend_items(items);
    }
    if !chase_current {
        return false;
    }
    // Pre-select the active session's row wherever it landed. The confirm
    // map resolves each row's filter key back to its id.
    let ids = Rc::clone(&scan.ids);
    let current = scan.current.clone();
    scan.select
        .borrow()
        .select_matching(|item| ids.borrow().get(&item.filter_key) == Some(&current))
}

/// Build one row: the truncated first user message (tagged `(current)` for
/// the active session) as the label, the metadata triplet as the dim
/// description, and `"{first_user_message} {session_id}"` as the filter
/// key so either the prompt or the id matches.
fn build_item(preview: &SessionPreview, is_current: bool, now: DateTime<Utc>) -> SelectItem {
    SelectItem::new(format_primary(preview, is_current), haystack(preview))
        .with_description(format_secondary(preview, now))
}

/// Searchable text for a preview: the first user message (when present)
/// plus the session id, so a substring of either finds the row.
fn haystack(preview: &SessionPreview) -> String {
    let first = preview.first_user_message.as_deref().unwrap_or("");
    format!("{first} {}", preview.session_id)
}

/// The confirm/close subtitle. Enter and Esc are the widget's built-in
/// keys (not rebindable actions), so they keep the fixed convention. Only
/// the labels resolve through the keybinding data.
fn subtitle() -> String {
    let confirm = confirm_key_label();
    let close = close_key_label();
    format!("{confirm} to resume  \u{2022}  {close} to close")
}

/// The primary (left) column: the first user message, truncated, with a
/// `(current)` suffix on the active session's row. Falls back to a
/// placeholder when the session has no user message yet.
fn format_primary(preview: &SessionPreview, is_current: bool) -> String {
    let raw = preview
        .first_user_message
        .as_deref()
        .unwrap_or("(no user message yet)");
    let one_line = raw.lines().next().unwrap_or(raw);
    let truncated = truncate_chars(one_line, PREVIEW_MAX_CHARS);
    if is_current {
        format!("{truncated} (current)")
    } else {
        truncated
    }
}

/// The secondary (right / description) column: message count, creation
/// date, and time since the last message, e.g.
/// `42 msgs · created May 8 · last 5m`. The session id is omitted (it's
/// already the row's value and would dominate the column width).
fn format_secondary(preview: &SessionPreview, now: DateTime<Utc>) -> String {
    let count = preview.message_count;
    let msg_word = if count == 1 { "msg" } else { "msgs" };
    let created = format_created(now, preview.created_at);
    let last = format_age(now, preview.last_message_at);
    format!("{count} {msg_word} · created {created} · last {last}")
}

/// Render `then` as a coarse age relative to `now`: `now / 5m / 3h / 2d /
/// 4w / 6mo / 2y`. The bucket boundaries are deliberately fuzzy.
pub(crate) fn format_age(now: DateTime<Utc>, then: DateTime<Utc>) -> String {
    let secs = now.signed_duration_since(then).num_seconds().max(0);
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

/// Render `created` as an adaptive absolute date relative to `now`:
/// clock-only for the same calendar day (`14:22`), month + day for the
/// same year (`May 8`), month + day + year otherwise (`May 8 2024`). Both
/// arguments are UTC, matching the UTC session-id mint format.
fn format_created(now: DateTime<Utc>, created: DateTime<Utc>) -> String {
    if now.date_naive() == created.date_naive() {
        created.format("%H:%M").to_string()
    } else if now.year() == created.year() {
        created.format("%b %-d").to_string()
    } else {
        created.format("%b %-d %Y").to_string()
    }
}

/// Truncate to `max` characters (not bytes), appending an ellipsis when
/// cut.
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max {
        return text.to_string();
    }
    let cut = max.saturating_sub(1).min(chars.len());
    let mut s: String = chars[..cut].iter().collect();
    s.push('\u{2026}');
    s
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use vaxis::vxfw::SelectStyles;

    use super::*;

    fn preview(
        session_id: &str,
        first_user: Option<&str>,
        count: usize,
        age: Duration,
    ) -> SessionPreview {
        let now = Utc::now();
        let last = now - age;
        SessionPreview {
            session_id: session_id.to_string(),
            modified: last,
            created_at: last,
            last_message_at: last,
            size_bytes: 1024,
            message_count: count,
            first_user_message: first_user.map(|s| s.to_string()),
        }
    }

    fn scan_over(
        previews: Vec<SessionPreview>,
        current: &str,
    ) -> (SessionScan, Rc<RefCell<Option<SessionRequest>>>) {
        let select = Rc::new(RefCell::new(FilterableSelect::new(
            loading_items(),
            SelectStyles::default(),
        )));
        let ids = Rc::new(RefCell::new(HashMap::new()));
        let request_slot: Rc<RefCell<Option<SessionRequest>>> = Rc::new(RefCell::new(None));
        {
            let mut sel = select.borrow_mut();
            let ids_c = Rc::clone(&ids);
            let current_c = current.to_string();
            let request_c = Rc::clone(&request_slot);
            sel.on_confirm = Some(Box::new(move |_ctx, item| {
                let Some(session_id) = ids_c.borrow().get(&item.filter_key).cloned() else {
                    return;
                };
                if session_id != current_c {
                    *request_c.borrow_mut() = Some(SessionRequest::Resume(session_id));
                }
            }));
        }
        let scan = SessionScan {
            select,
            current: current.to_string(),
            ids,
        };
        extend_session_scan(&scan, &previews, Utc::now(), true, true);
        (scan, request_slot)
    }

    #[test]
    fn build_item_tags_the_current_row_and_indexes_both_prompt_and_id() {
        let p = preview(
            "2025-05-09",
            Some("debug the streaming protocol"),
            17,
            Duration::hours(3),
        );
        let current = build_item(&p, true, Utc::now());
        assert!(current.label.contains("debug the streaming protocol"));
        assert!(current.label.ends_with("(current)"), "{}", current.label);
        // The filter key carries both the prompt and the id.
        assert!(current.filter_key.contains("debug the streaming protocol"));
        assert!(current.filter_key.contains("2025-05-09"));

        let other = build_item(&p, false, Utc::now());
        assert!(!other.label.contains("(current)"), "{}", other.label);
    }

    #[test]
    fn confirming_a_different_row_parks_resume() {
        let previews = vec![
            preview("2025-05-10", Some("newest"), 1, Duration::minutes(1)),
            preview("2025-05-09", Some("older"), 1, Duration::hours(1)),
        ];
        let (scan, request) = scan_over(previews, "2025-05-09");
        // The current row (older) is pre-selected; move down onto the
        // first row and confirm it.
        scan.select
            .borrow()
            .select_matching(|item| item.filter_key.contains("2025-05-10"));
        let picked = scan.select.borrow().selected().expect("a row is selected");
        // Fire the confirm callback directly with the picked item.
        if let Some(cb) = scan.select.borrow_mut().on_confirm.as_mut() {
            let mut ctx = vaxis::vxfw::EventContext::new();
            cb(&mut ctx, &picked);
        }
        assert!(
            matches!(request.borrow().as_ref(), Some(SessionRequest::Resume(id)) if id == "2025-05-10"),
            "parked a resume for the picked id: {:?}",
            request.borrow().as_ref().map(|_| ()),
        );
    }

    #[test]
    fn confirming_the_current_row_parks_nothing() {
        let previews = vec![preview("2025-05-09", Some("only"), 1, Duration::hours(1))];
        let (scan, request) = scan_over(previews, "2025-05-09");
        let picked = scan.select.borrow().selected().expect("a row is selected");
        if let Some(cb) = scan.select.borrow_mut().on_confirm.as_mut() {
            let mut ctx = vaxis::vxfw::EventContext::new();
            cb(&mut ctx, &picked);
        }
        assert!(request.borrow().is_none(), "the current row is a no-op");
    }

    /// Drive the REAL confirm closure `open_session_selector` builds, over a
    /// live overlay stack, having filled the list and selected a non-current
    /// row. Returns the parked request, the stack (to check open/closed), and
    /// the toast stack.
    #[expect(clippy::type_complexity)]
    fn confirm_switch_over(
        busy: bool,
    ) -> (
        Rc<RefCell<Option<SessionRequest>>>,
        Rc<RefCell<crate::overlay::OverlayStack>>,
        crate::toasts::ToastStack,
    ) {
        let handles = OverlayHandles::for_tests();
        handles.busy.set(busy);

        open_session_selector(&handles, "current".to_string());
        let scan = handles
            .session_scan
            .borrow_mut()
            .take()
            .expect("open parked a scan");
        let previews = vec![preview(
            "other",
            Some("other prompt"),
            1,
            Duration::hours(1),
        )];
        extend_session_scan(&scan, &previews, Utc::now(), true, true);
        scan.select
            .borrow()
            .select_matching(|item| item.filter_key.contains("other"));
        let picked = scan.select.borrow().selected().expect("a row is selected");
        if let Some(cb) = scan.select.borrow_mut().on_confirm.as_mut() {
            let mut ctx = vaxis::vxfw::EventContext::new();
            cb(&mut ctx, &picked);
        }
        (
            Rc::clone(&handles.session_request),
            Rc::clone(&handles.stack),
            Rc::clone(&handles.toasts),
        )
    }

    /// Live work does not hold the user in a session: the one they leave keeps
    /// folding in the background, so a busy switch parks and closes exactly
    /// like an idle one and raises no refusal.
    #[test]
    fn confirm_switch_while_busy_parks_and_closes() {
        let (request, stack, toasts) = confirm_switch_over(true);
        assert!(
            matches!(request.borrow().as_ref(), Some(SessionRequest::Resume(id)) if id == "other"),
            "a busy switch parks a resume for the picked id",
        );
        assert!(!stack.borrow().is_open(), "the confirm closed the overlay");
        assert!(
            crate::toasts::toast_texts(&toasts).is_empty(),
            "and said nothing about being busy: {:?}",
            crate::toasts::toast_texts(&toasts),
        );
    }

    /// While idle, confirming a non-current row parks the resume and closes.
    #[test]
    fn confirm_switch_while_idle_parks_and_closes() {
        let (request, stack, toasts) = confirm_switch_over(false);
        assert!(
            matches!(request.borrow().as_ref(), Some(SessionRequest::Resume(id)) if id == "other"),
            "an idle switch parks a resume for the picked id",
        );
        assert!(!stack.borrow().is_open(), "the confirm closed the overlay");
        assert!(
            crate::toasts::toast_texts(&toasts).is_empty(),
            "no toast raised while idle"
        );
    }

    /// Batches accumulate across a streamed fill: the first replaces the
    /// loading placeholder, later batches append, and the current-session
    /// chase lands the highlight once its row streams in.
    #[test]
    fn streaming_appends_batches_and_chases_current_across_them() {
        let select = Rc::new(RefCell::new(FilterableSelect::new(
            loading_items(),
            SelectStyles::default(),
        )));
        let ids = Rc::new(RefCell::new(HashMap::new()));
        let scan = SessionScan {
            select,
            current: "2025-05-08".to_string(),
            ids,
        };

        // First batch: two newer sessions, no current row. Replaces the
        // placeholder and lands the cursor on the first row.
        let batch1 = vec![
            preview("2025-05-10", Some("newest"), 1, Duration::minutes(1)),
            preview("2025-05-09", Some("second"), 1, Duration::hours(1)),
        ];
        let found = extend_session_scan(&scan, &batch1, Utc::now(), true, true);
        assert!(!found, "current is not in the first batch");
        assert_eq!(scan.select.borrow().visible_labels().len(), 2);
        assert!(
            scan.selected_filter_key().unwrap().contains("2025-05-10"),
            "cursor on the first streamed row"
        );

        // Second batch carries the current session; the chase selects it.
        let batch2 = vec![preview(
            "2025-05-08",
            Some("current one"),
            1,
            Duration::hours(2),
        )];
        let found = extend_session_scan(&scan, &batch2, Utc::now(), false, true);
        assert!(found, "current row found and selected in the second batch");
        assert_eq!(
            scan.select.borrow().visible_labels().len(),
            3,
            "rows accumulated across batches"
        );
        assert!(scan.selected_filter_key().unwrap().contains("2025-05-08"));
    }

    /// With the chase disabled (the host saw the user navigate), a later
    /// batch carrying the current session does not move the selection.
    #[test]
    fn streaming_without_chase_keeps_the_user_selection() {
        let select = Rc::new(RefCell::new(FilterableSelect::new(
            loading_items(),
            SelectStyles::default(),
        )));
        let ids = Rc::new(RefCell::new(HashMap::new()));
        let scan = SessionScan {
            select,
            current: "2025-05-08".to_string(),
            ids,
        };

        let batch1 = vec![
            preview("2025-05-10", Some("newest"), 1, Duration::minutes(1)),
            preview("2025-05-09", Some("second"), 1, Duration::hours(1)),
        ];
        extend_session_scan(&scan, &batch1, Utc::now(), true, true);
        // The user navigates to the second row.
        scan.select
            .borrow()
            .select_matching(|item| item.filter_key.contains("2025-05-09"));
        let anchor = scan.selected_filter_key();

        // Current streams in, but the chase is off, so the selection stays.
        let batch2 = vec![preview(
            "2025-05-08",
            Some("current one"),
            1,
            Duration::hours(2),
        )];
        let found = extend_session_scan(&scan, &batch2, Utc::now(), false, false);
        assert!(!found, "chase disabled reports no selection change");
        assert_eq!(
            scan.selected_filter_key(),
            anchor,
            "the user's selection stayed put"
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
    fn secondary_column_carries_count_created_and_last() {
        let now = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_opt(20, 0, 0)
            .unwrap()
            .and_utc();
        let p = SessionPreview {
            session_id: "2025-05-11-13-22-00-000".into(),
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
        assert_eq!(
            format_secondary(&p, now),
            "42 msgs · created 13:22 · last 2h"
        );
    }

    #[test]
    fn long_preview_truncates_with_ellipsis() {
        let long = "a".repeat(200);
        let p = preview("2025-05-11", Some(&long), 1, Duration::seconds(10));
        let primary = format_primary(&p, false);
        assert!(primary.ends_with('\u{2026}'), "{primary}");
        assert_eq!(primary.chars().count(), PREVIEW_MAX_CHARS);
    }

    /// The confirm/close subtitle resolves its key labels from the
    /// keybinding data, so a rebind moves both the rendered hint and the
    /// assertion together rather than tracking a literal.
    #[test]
    fn subtitle_resolves_confirm_and_close_labels() {
        let s = subtitle();
        assert!(s.contains(&confirm_key_label()), "{s}");
        assert!(s.contains(&close_key_label()), "{s}");
    }
}
