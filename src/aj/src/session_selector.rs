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
//! A row's filter key is `"{first_user_message} {tag} {session_id}"` so typing
//! the prompt, the label, or the id finds it, and a `#`-prefixed query narrows
//! to the labels alone. The confirmed value is the session id, recovered
//! through a shared filter-key -> id map (the same indirection the command
//! palette uses for its actions), since the widget hands the confirm callback
//! only the row's filter key.

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
use crate::text::one_line;

/// How much of the first user message a row's primary column shows before
/// truncating with an ellipsis.
const PREVIEW_MAX_CHARS: usize = 60;

/// How wide the tag column may get before truncating with an ellipsis. A tag
/// can be 80 bytes, and the column is sized to the widest one on show, so
/// without a cap a single long label would push the preview off the overlay.
const TAG_COLUMN_MAX_CHARS: usize = 16;

/// Typed as the query's first character, this narrows the filter to the tags
/// (see [`FilterableSelect::set_scope_sigil`]). Anywhere else it is ordinary
/// text.
const TAG_SCOPE_SIGIL: char = '#';

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
        sel.set_scope_sigil(TAG_SCOPE_SIGIL);
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
/// the active session) as the label, the session's own label as a column
/// beside it, the metadata triplet as the dim description, and
/// `"{first_user_message} {tag} {session_id}"` as the filter key so the
/// prompt, the label, or the id all match.
///
/// The tag is the row's scope key too, so a `#`-prefixed query matches labels
/// and nothing else. It is folded to one line before any of that: it comes
/// from a file that may have been hand-edited, and a lone carriage return in a
/// drawn row is a panic in the frame (see [`one_line`]). Folding once here is
/// what keeps the drawn column and the text the filter matches identical.
fn build_item(preview: &SessionPreview, is_current: bool, now: DateTime<Utc>) -> SelectItem {
    let tag = preview.tag.as_deref().map(one_line);
    let item = SelectItem::new(
        format_primary(preview, is_current),
        haystack(preview, tag.as_deref()),
    )
    .with_description(format_secondary(preview, now));
    match tag {
        Some(tag) => item
            .with_prefix(truncate_chars(&tag, TAG_COLUMN_MAX_CHARS))
            .with_scope_key(tag),
        None => item,
    }
}

/// Searchable text for a preview: the first user message (when present), the
/// user's tag, and the session id, so a substring of any of them finds the
/// row.
fn haystack(preview: &SessionPreview, tag: Option<&str>) -> String {
    let first = preview.first_user_message.as_deref().unwrap_or("");
    let tag = tag.unwrap_or("");
    format!("{first} {tag} {}", preview.session_id)
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
///
/// The message is a whole prompt, so only its first line is shown, and that
/// line is folded like every other value drawn here (see [`one_line`]).
fn format_primary(preview: &SessionPreview, is_current: bool) -> String {
    let raw = preview
        .first_user_message
        .as_deref()
        .unwrap_or("(no user message yet)");
    let first_line = one_line(raw.lines().next().unwrap_or(raw));
    let truncated = truncate_chars(&first_line, PREVIEW_MAX_CHARS);
    if is_current {
        format!("{truncated} (current)")
    } else {
        truncated
    }
}

/// The secondary (right / description) column: message count, creation date,
/// and time since the last message, e.g. `42 msgs · created May 8 · last 5m`.
/// The session id is omitted (it's already the row's value and would dominate
/// the column width), and the tag has a column of its own.
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
            tag: None,
        }
    }

    /// The same preview, labelled: what the scan produces for a session whose
    /// sidecar holds a tag.
    fn tagged(preview: SessionPreview, tag: &str) -> SessionPreview {
        SessionPreview {
            tag: Some(tag.to_string()),
            ..preview
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
        let current = build_item(&tagged(p.clone(), "fix-auth"), true, Utc::now());
        assert!(current.label.contains("debug the streaming protocol"));
        assert!(current.label.ends_with("(current)"), "{}", current.label);
        // The filter key carries the prompt, the label, and the id.
        assert!(current.filter_key.contains("debug the streaming protocol"));
        assert!(current.filter_key.contains("fix-auth"));
        assert!(current.filter_key.contains("2025-05-09"));

        let other = build_item(&p, false, Utc::now());
        assert!(!other.label.contains("(current)"), "{}", other.label);
        assert!(
            !other.filter_key.contains("fix-auth"),
            "an untagged row indexes no label: {}",
            other.filter_key,
        );
    }

    /// The tag supplements the row rather than displacing anything: it takes a
    /// column of its own, the preview keeps the label, and the metadata column
    /// is the metadata alone. An untagged row carries no column at all, so an
    /// untagged project's list is unchanged.
    #[test]
    fn the_tag_is_a_column_of_its_own_beside_the_preview() {
        let p = preview(
            "2025-05-09",
            Some("debug the protocol"),
            42,
            Duration::hours(2),
        );
        let item = build_item(&tagged(p.clone(), "fix-auth"), false, Utc::now());
        assert_eq!(item.prefix.as_deref(), Some("fix-auth"));
        assert_eq!(item.label, "debug the protocol");
        assert_eq!(item.scope_key.as_deref(), Some("fix-auth"));
        assert!(
            item.description
                .as_deref()
                .is_some_and(|d| d.starts_with("42 msgs · created ")),
            "the metadata column is the metadata: {:?}",
            item.description,
        );

        let untagged = build_item(&p, false, Utc::now());
        assert_eq!(untagged.prefix, None);
        assert_eq!(untagged.scope_key, None);
        assert_eq!(untagged.description, item.description);
    }

    /// A tag can be 80 bytes, so the column truncates. The filter still sees
    /// the whole label, so a query for the part that was cut off finds the row.
    #[test]
    fn a_long_tag_truncates_in_its_column_but_not_in_the_filter() {
        let long = "release-candidate-verification";
        let p = tagged(
            preview("2025-05-09", Some("prompt"), 1, Duration::hours(1)),
            long,
        );
        let item = build_item(&p, false, Utc::now());
        let column = item.prefix.expect("a tagged row has the column");
        assert_eq!(column.chars().count(), TAG_COLUMN_MAX_CHARS);
        assert!(column.ends_with('\u{2026}'), "{column}");
        assert_eq!(item.scope_key.as_deref(), Some(long));
        assert!(item.filter_key.contains(long));
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
            tag: None,
        };
        assert_eq!(
            format_secondary(&p, now),
            "42 msgs · created 13:22 · last 2h"
        );
        assert_eq!(
            format_secondary(&tagged(p, "fix-auth"), now),
            "42 msgs · created 13:22 · last 2h",
            "the label has a column of its own and does not lead this one",
        );
    }

    /// A tag from a hand-edited sidecar is folded before it reaches the
    /// column, and a row built from one draws. The store's own write path
    /// rejects a control character, but the file it writes can be edited
    /// afterwards, and a lone carriage return in a drawn row is a panic in the
    /// frame rather than a stray glyph.
    #[test]
    fn a_control_character_in_a_stored_tag_never_reaches_the_column() {
        use vaxis::vxfw::{FilterableSelect, SelectStyles, Widget};

        let p = tagged(
            preview("2025-05-09-14-30-00", Some("debug"), 42, Duration::hours(2)),
            "ab\rcd",
        );
        let item = build_item(&p, false, Utc::now());
        assert_eq!(item.prefix.as_deref(), Some("abcd"));
        assert_eq!(item.scope_key.as_deref(), Some("abcd"));
        assert!(!item.filter_key.contains('\r'), "{}", item.filter_key);

        let mut select = FilterableSelect::new(vec![item], SelectStyles::default());
        let surface = select.draw(&crate::test_support::draw_ctx(60, Some(10)));
        assert!(!surface.children.is_empty(), "the row drew");
    }

    #[test]
    fn long_preview_truncates_with_ellipsis() {
        let long = "a".repeat(200);
        let p = preview("2025-05-11", Some(&long), 1, Duration::seconds(10));
        let primary = format_primary(&p, false);
        assert!(primary.ends_with('\u{2026}'), "{primary}");
        assert_eq!(primary.chars().count(), PREVIEW_MAX_CHARS);
    }

    /// A store holding one tagged session: an empty log under a valid id, the
    /// state a session is in before its first prompt, plus its tag sidecar.
    /// The `TempDir` guard is returned so the store outlives the caller's use
    /// of it.
    fn tagged_store(
        tag: &str,
    ) -> (
        tempfile::TempDir,
        aj_session::ConversationPersistence,
        String,
    ) {
        let dir = tempfile::TempDir::with_prefix("aj-selector-tag-").expect("temp dir");
        let persistence = aj_session::ConversationPersistence::new(dir.path().to_path_buf());
        let id = "2025-05-09-14-30-00-000".to_string();
        std::fs::write(dir.path().join(format!("{id}.jsonl")), "").expect("session log");
        persistence.write_tag(&id, Some(tag)).expect("tag sidecar");
        (dir, persistence, id)
    }

    /// The selector's rows carry the labels its own scan read, so a tagged
    /// session shows its tag even though nothing has enumerated it into the
    /// directory the sidebar draws.
    #[test]
    fn a_tag_the_directory_never_saw_still_reaches_the_selector() {
        let (_dir, persistence, id) = tagged_store("held-elsewhere");
        let mut previews = Vec::new();
        persistence.list_session_previews_streaming(&|| false, &mut |batch| {
            previews.extend(batch);
        });
        assert_eq!(previews.len(), 1, "the store scan found the session");
        assert_eq!(previews[0].session_id, id);

        let handles = OverlayHandles::for_tests();
        open_session_selector(&handles, "another-session".to_string());
        let scan = handles
            .session_scan
            .borrow_mut()
            .take()
            .expect("open parked a scan");
        extend_session_scan(&scan, &previews, Utc::now(), true, true);

        let drawn = drawn_rows(&handles).join("\n");
        assert!(
            drawn.contains("held-elsewhere"),
            "the drawn row carries the label the scan read: {drawn}",
        );
    }

    /// The composed overlay's drawn rows: the window the stack holds, drawn
    /// and composited the way a frame paints it.
    fn drawn_rows(handles: &OverlayHandles) -> Vec<String> {
        let stack = handles.stack.borrow();
        let window = &stack.top().expect("the selector is open").widget;
        let surface = window
            .borrow_mut()
            .draw(&crate::test_support::draw_ctx(90, Some(20)));
        crate::test_support::rows(&surface)
    }

    /// Three rows the filter can tell apart: two labelled, one not, with
    /// `fix` reachable through a prompt as well as through a label.
    fn filter_previews() -> Vec<SessionPreview> {
        vec![
            tagged(
                preview(
                    "2025-05-10-00-00-00",
                    Some("refactor the parser"),
                    3,
                    Duration::minutes(1),
                ),
                "fix-auth",
            ),
            preview(
                "2025-05-09-00-00-00",
                Some("fix the streaming bug"),
                2,
                Duration::hours(1),
            ),
            tagged(
                preview(
                    "2025-05-08-00-00-00",
                    Some("write the docs"),
                    1,
                    Duration::hours(4),
                ),
                "eval-run",
            ),
        ]
    }

    /// Open the real selector over `previews` and type `query` into it one
    /// key at a time, through the widget the overlay stack hands focus to.
    /// Nothing here reaches past the composed overlay, so dropping the scope
    /// wiring or the tag column shows up in the drawn rows.
    fn selector_over(previews: &[SessionPreview], query: &str) -> (OverlayHandles, SessionScan) {
        use vaxis::key::Key;
        use vaxis::vxfw::{Event, EventContext, Phase};

        let handles = OverlayHandles::for_tests();
        open_session_selector(&handles, "current".to_string());
        let scan = handles
            .session_scan
            .borrow_mut()
            .take()
            .expect("open parked a scan");
        extend_session_scan(&scan, previews, Utc::now(), true, true);

        let focus = Rc::clone(&handles.stack.borrow().top().expect("open").focus);
        for c in query.chars() {
            let event = Event::KeyPress(Key {
                codepoint: u32::from(c),
                text: Some(c.to_string().into()),
                ..Key::default()
            });
            let mut ctx = EventContext::new();
            ctx.phase = Phase::AtTarget;
            focus.borrow_mut().handle_event(&mut ctx, &event);
        }
        (handles, scan)
    }

    /// The labels of the rows a query left visible, in rank order.
    fn matched(previews: &[SessionPreview], query: &str) -> Vec<String> {
        let (_handles, scan) = selector_over(previews, query);
        // Bound to a name so the `Ref` is released before `scan` drops.
        let labels = scan.select.borrow().visible_labels();
        labels
    }

    /// The tag joins the corpus the plain query already searched, so an id, a
    /// prompt and a label all find their row with no syntax to learn.
    #[test]
    fn a_plain_query_matches_ids_previews_and_tags_alike() {
        let previews = filter_previews();
        assert_eq!(matched(&previews, "refactor"), ["refactor the parser"]);
        assert_eq!(matched(&previews, "2025-05-08"), ["write the docs"]);
        assert_eq!(matched(&previews, "eval-run"), ["write the docs"]);
        assert_eq!(
            matched(&previews, "fix"),
            ["refactor the parser", "fix the streaming bug"],
            "unscoped, a label and a prompt are equally good matches",
        );
    }

    /// The `#` prefix narrows to the labels: the row whose only `fix` is in
    /// its prompt drops out, and so does every unlabelled row.
    #[test]
    fn a_hash_prefixed_query_matches_tags_only() {
        let previews = filter_previews();
        assert_eq!(matched(&previews, "#fix"), ["refactor the parser"]);
        assert_eq!(
            matched(&previews, "#refactor"),
            Vec::<String>::new(),
            "the prompt is out of scope under the sigil",
        );
    }

    /// A bare `#` is the empty scoped query, so it lists the labelled
    /// sessions and nothing else.
    #[test]
    fn a_bare_hash_lists_the_labelled_sessions() {
        assert_eq!(
            matched(&filter_previews(), "#"),
            ["refactor the parser", "write the docs"],
        );
    }

    /// A project where nothing is labelled has nothing in scope, so a `#`
    /// query comes up empty instead of falling back to the corpus.
    #[test]
    fn a_hash_query_over_an_unlabelled_project_matches_nothing() {
        let previews = vec![
            preview(
                "2025-05-10-00-00-00",
                Some("refactor"),
                1,
                Duration::hours(1),
            ),
            preview("2025-05-09-00-00-00", Some("debug"), 1, Duration::hours(2)),
        ];
        assert_eq!(matched(&previews, "#"), Vec::<String>::new());
        assert_eq!(matched(&previews, "#refactor"), Vec::<String>::new());
    }

    /// Only the leading `#` is a sigil. Anywhere else it is a character like
    /// any other, matched against the corpus, so a prompt that quotes an
    /// issue number is still findable.
    #[test]
    fn a_hash_inside_a_query_is_literal() {
        let previews = vec![
            preview(
                "2025-05-10-00-00-00",
                Some("close issue #42"),
                1,
                Duration::hours(1),
            ),
            tagged(
                preview(
                    "2025-05-09-00-00-00",
                    Some("unrelated"),
                    1,
                    Duration::hours(2),
                ),
                "issue-42",
            ),
        ];
        assert_eq!(matched(&previews, "issue #4"), ["close issue #42"]);
    }

    /// The tag column is drawn, and drawn beside the preview rather than in
    /// place of it. This goes through `open_session_selector` and the window
    /// it pushes, so the column surviving in `build_item` alone is not enough
    /// to pass.
    #[test]
    fn the_tag_column_draws_beside_the_preview() {
        let (handles, _scan) = selector_over(&filter_previews(), "");
        let rows = drawn_rows(&handles);
        let row = rows
            .iter()
            .find(|row| row.contains("refactor the parser"))
            .expect("the labelled row drew");
        let tag_at = row.find("fix-auth").expect("the tag column drew");
        let preview_at = row.find("refactor the parser").expect("checked above");
        assert!(tag_at < preview_at, "tag column left of the preview: {row}");
        assert!(
            row.contains("3 msgs · created "),
            "and the metadata column is still there: {row}",
        );

        // The unlabelled row keeps its preview, indented into the same column.
        let plain = rows
            .iter()
            .find(|row| row.contains("fix the streaming bug"))
            .expect("the unlabelled row drew");
        assert_eq!(
            plain.find("fix the streaming bug"),
            Some(preview_at),
            "every row's preview starts in the same column: {plain}",
        );
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
