//! The session-selector overlay: resume a previous session in place.
//!
//! A [`FilterableSelect`] over the sessions available to this frontend.
//! Confirming any session row parks a [`SessionRequest::Resume`] and closes
//! the overlay. The frontend treats the current session as a no-op unless
//! retrying a refused attachment. Esc cancels without a request.
//!
//! Every selector opens synchronously over a directory snapshot, then enriches
//! its rows with host previews. The active session is pre-selected and marked
//! with the sidebar's focus marker before any previews arrive.
//!
//! Each row's filter key contains the full first prompt, tag, session id, and
//! host label when supplied by the directory. A `#`-prefixed query narrows
//! to tags alone. The confirmed value is the session id, recovered
//! through a shared filter-key -> id map (the same indirection the command
//! palette uses for its actions), since the widget hands the confirm callback
//! only the row's filter key.
//!
//! Archived sessions are left out, the one the user is in excepted, and the
//! overlay's own toggle ([`ACTION_SESSION_TOGGLE_ARCHIVED`]) puts the rest
//! back inline, marked. A picker is where hiding them earns its keep, so the
//! toggle belongs to this overlay rather than to whatever the strip is
//! showing. The source retains every preview it has received, revealed or not, so
//! toggling never starts another read.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use aj_app::keybindings::{ACTION_SESSION_TOGGLE_ARCHIVED, action_shortcut};
use aj_app::session::SessionRequest;
use aj_session::SessionPreview;
use aj_wire::{DirectoryHost, SessionSummary};
use chrono::{DateTime, Datelike, Utc};
use vaxis::vxfw::{
    DrawContext, Event, EventContext, FilterableSelect, OverlayWindow, RelativePoint, SelectColumn,
    SelectItem, SubSurface, Surface, TextAlign, Widget, draw_widget, to_widget_ref,
};

use crate::interactive::OverlayHandles;
use crate::keymap::action_matches;
use crate::overlay::{
    OverlayPlacement, OverlayStack, close_all, close_key_label, close_top, confirm_key_label,
};
use crate::settings_ui::push_window;
use crate::sidebar::{
    FOCUS_MARKER, LOCKED_MARKER, host_label, session_label, session_label_source,
};
use crate::text::one_line;

/// Maximum length of an id-derived fallback label.
const FALLBACK_LABEL_MAX_CHARS: usize = 60;

/// How wide the tag column may get before truncating with an ellipsis. A tag
/// can be 80 bytes, and the column is sized to the widest one on show, so
/// without a cap a single long label would push the preview off the overlay.
const TAG_COLUMN_MAX_CHARS: usize = 16;

/// Typed as the query's first character, this narrows the filter to the tags
/// (see [`FilterableSelect::set_scope_sigil`]). Anywhere else it is ordinary
/// text.
const TAG_SCOPE_SIGIL: char = '#';

/// A parked request for the host to scan session previews and fill the
/// selector's list. The select handle is `!Send`, so it stays on the UI
/// thread. The spawned scan produces only the (Send) previews.
pub(crate) struct SessionScan {
    pub(crate) select: Rc<RefCell<FilterableSelect>>,
    /// The active session id, for its leading marker and pre-selection.
    current: String,
    /// filter_key -> session_id, indexed on open and preview updates, and read
    /// by the confirm callback (which sees only the row's filter key).
    ids: Rc<RefCell<HashMap<String, String>>>,
    /// Every preview delivered so far, archived ones included, shared with
    /// the widget so its toggle can rebuild the rows without rescanning.
    rows: SelectorRows,
    /// Whether archived sessions are being shown, flipped by the widget's
    /// toggle and read here so a batch arriving after it lands filters the
    /// same way.
    reveal: Rc<Cell<bool>>,
    window: Option<Rc<RefCell<OverlayWindow>>>,
}

impl SessionScan {
    pub(crate) fn is_open(&self, stack: &OverlayStack) -> bool {
        stack
            .top()
            .is_some_and(|overlay| Rc::ptr_eq(&overlay.focus, &self.select.borrow().focus_target()))
    }

    pub(crate) fn finish(&self, failed: bool) {
        if let Some(window) = &self.window {
            window.borrow_mut().title = if failed {
                "Resume session · preview search incomplete"
            } else {
                "Resume session"
            }
            .to_string();
        }
    }
}

/// Rows the shared selector widget can rebuild when its archived toggle
/// changes. The directory owns membership, order, tags, and archive state.
#[derive(Clone)]
struct SelectorRows {
    rows: Vec<SessionSummary>,
    hosts: Vec<DirectoryHost>,
    previews: Rc<RefCell<Vec<SessionPreview>>>,
}

impl SelectorRows {
    fn items(
        &self,
        ids: &Rc<RefCell<HashMap<String, String>>>,
        current: &str,
        reveal: bool,
        now: DateTime<Utc>,
    ) -> Vec<SelectItem> {
        build_items(
            ids,
            &self.rows,
            &self.hosts,
            &self.previews.borrow(),
            current,
            reveal,
            now,
        )
    }
}

/// Open over a directory snapshot and park a preview scan in
/// `handles.session_scan`. Basic rows remain selectable while previews load
/// or when a host cannot supply them. Reopening refreshes both the directory
/// snapshot and previews. Any confirmed session row
/// lands a [`SessionRequest::Resume`] in `handles.session_request` and closes
/// the overlay. Esc closes without a request. Does not move focus: the caller
/// posts the refocus event.
///
/// A switch is never refused for being busy. The session left behind stays
/// attached and keeps folding, so its turn finishes unwatched.
pub(crate) fn open_session_selector(
    handles: &OverlayHandles,
    current: String,
    rows: &[SessionSummary],
    hosts: &[DirectoryHost],
) {
    let rows = SelectorRows {
        rows: rows.to_vec(),
        hosts: hosts.to_vec(),
        previews: Rc::new(RefCell::new(Vec::new())),
    };
    let ids: Rc<RefCell<HashMap<String, String>>> = Rc::new(RefCell::new(HashMap::new()));
    let reveal = Rc::new(Cell::new(false));
    let initial = rows.items(&ids, &current, reveal.get(), Utc::now());
    let select = Rc::new(RefCell::new(FilterableSelect::new(
        initial,
        handles.chrome.select.clone(),
    )));
    select.borrow_mut().set_literal_search(true);
    select.borrow().select_matching(|item| {
        ids.borrow().get(&item.filter_key).map(String::as_str) == Some(current.as_str())
    });
    let focus = select.borrow().focus_target();
    {
        let mut sel = select.borrow_mut();
        // A project can hold many sessions, so show the vertical scroll bar.
        sel.set_show_scrollbar(true);
        sel.set_scope_sigil(TAG_SCOPE_SIGIL);
        // Keep enough of the prompt to recognize a session, even when metadata
        // needs clipping in a narrow overlay or beside a long host name.
        sel.set_label_reserve(24);
        let ids_c = Rc::clone(&ids);
        let request_c = Rc::clone(&handles.session_request);
        let stack_c = Rc::clone(&handles.stack);
        let editor_c = Rc::clone(&handles.editor);
        sel.on_confirm = Some(Box::new(move |ctx, item| {
            let Some(session_id) = ids_c.borrow().get(&item.filter_key).cloned() else {
                return;
            };
            // The frontend decides whether selecting the current session is a
            // no-op or an explicit retry of a refused attachment.
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
    let selector = Rc::new(RefCell::new(SessionSelector {
        select: Rc::clone(&select),
        ids: Rc::clone(&ids),
        rows: rows.clone(),
        reveal: Rc::clone(&reveal),
        current: current.clone(),
        window: None,
    }));
    let window = push_window(
        &handles.stack,
        &handles.chrome,
        "Resume session · loading previews",
        subtitle(false),
        to_widget_ref(Rc::clone(&selector)),
        focus,
        OverlayPlacement::Large,
    );
    selector.borrow_mut().window = Some(Rc::downgrade(&window));
    *handles.session_scan.borrow_mut() = Some(SessionScan {
        select,
        current,
        ids,
        rows,
        reveal,
        window: Some(window),
    });
}

/// The selector widget: the list, and what the archived toggle needs to
/// rebuild it in place.
///
/// It wraps the [`FilterableSelect`] rather than replacing it, for one chord
/// the select has no hook for. Everything else (typing, navigation, Enter,
/// Esc) falls through to the select underneath, which still owns them.
pub(crate) struct SessionSelector {
    select: Rc<RefCell<FilterableSelect>>,
    ids: Rc<RefCell<HashMap<String, String>>>,
    rows: SelectorRows,
    reveal: Rc<Cell<bool>>,
    current: String,
    /// The window frame, for the subtitle the toggle rewrites. `None` until
    /// the push that creates it returns. The frame owns this widget, so the
    /// back-reference must not retain it and its previews after closing.
    window: Option<Weak<RefCell<OverlayWindow>>>,
}

impl SessionSelector {
    /// Rebuild the rows for the current setting of the toggle, keeping the
    /// highlight on the row it was on when that row survives.
    ///
    /// From the retained source rows, so revealing starts no read. A preview
    /// batch still to arrive fills in behind this through
    /// [`extend_session_scan`], which reads the same flag.
    fn rebuild(&self, now: DateTime<Utc>) {
        let was = self.select.borrow().selected().map(|item| item.filter_key);
        let items = self
            .rows
            .items(&self.ids, &self.current, self.reveal.get(), now);
        {
            let select = self.select.borrow();
            select.set_items(items);
            if let Some(key) = was {
                select.select_matching(|item| item.filter_key == key);
            }
        }
        if let Some(window) = self.window.as_ref().and_then(Weak::upgrade) {
            window.borrow_mut().subtitle = subtitle(self.reveal.get());
        }
    }
}

impl Widget for SessionSelector {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // The select is drawn as a child so both identities stay on the focus
        // path: returning its surface bare would let the caller re-stamp it
        // with this widget's identity and drop the select, and its Enter and
        // Esc with it.
        let mut surface = Surface::with_size(ctx.max.size());
        surface.children.push(SubSurface {
            origin: RelativePoint { row: 0, col: 0 },
            surface: draw_widget(&to_widget_ref(Rc::clone(&self.select)), ctx),
            z_index: 0,
        });
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        let Event::KeyPress(key) = event else {
            return;
        };
        if action_matches(key, ACTION_SESSION_TOGGLE_ARCHIVED) {
            self.reveal.set(!self.reveal.get());
            self.rebuild(Utc::now());
            ctx.consume_and_redraw();
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

/// Enrich directory rows in place, preserving the selected row and scroll.
/// Retain hidden previews too, so archive reveal never requests another read.
pub(crate) fn extend_session_scan(
    scan: &SessionScan,
    previews: &[SessionPreview],
    now: DateTime<Utc>,
) {
    scan.rows.previews.borrow_mut().extend_from_slice(previews);
    let select = scan.select.borrow();
    for preview in previews {
        let Some((index, row)) = scan
            .rows
            .rows
            .iter()
            .filter(|row| shown(row, &scan.current, scan.reveal.get()))
            .enumerate()
            .find(|(_, row)| row.id == preview.session_id)
        else {
            continue;
        };
        // Membership and order belong to the snapshot. Updating by source
        // index also keeps a highlighted row anchored when its search key changes.
        let item = build_item(row, &scan.rows.hosts, Some(preview), &scan.current, now);
        scan.ids
            .borrow_mut()
            .insert(item.filter_key.clone(), row.id.clone());
        select.update_item(index, item);
    }
}

/// Build selector rows from the client-side directory snapshot. The directory
/// supplies fallback rows until previews arrive. Prompt text enriches both
/// the label and the search corpus without changing the routing identity.
fn build_items(
    ids: &Rc<RefCell<HashMap<String, String>>>,
    rows: &[SessionSummary],
    hosts: &[DirectoryHost],
    previews: &[SessionPreview],
    current: &str,
    reveal: bool,
    now: DateTime<Utc>,
) -> Vec<SelectItem> {
    let previews: HashMap<_, _> = previews
        .iter()
        .map(|p| (p.session_id.as_str(), p))
        .collect();
    let mut ids = ids.borrow_mut();
    ids.clear();
    rows.iter()
        .filter(|row| shown(row, current, reveal))
        .map(|row| {
            let preview = previews.get(row.id.as_str()).copied();
            let item = build_item(row, hosts, preview, current, now);
            ids.insert(item.filter_key.clone(), row.id.clone());
            item
        })
        .collect()
}

/// Whether a directory row is listed. Archived rows hide unless revealed, and
/// the session the user is in stays listed whatever its bit says.
fn shown(row: &SessionSummary, current: &str, reveal: bool) -> bool {
    reveal || !row.archived || row.id == current
}

/// Build one row from the fields an enumeration is allowed to carry, plus the
/// preview once its host has answered. The complete id remains the confirm
/// value and filter corpus even when its host qualifier is removed for display.
fn build_item(
    row: &SessionSummary,
    hosts: &[DirectoryHost],
    preview: Option<&SessionPreview>,
    current: &str,
    now: DateTime<Utc>,
) -> SelectItem {
    let tag = row.tag.as_deref().map(one_line);
    let host = row.host.as_deref().map(|id| {
        let label = hosts
            .iter()
            .find(|host| host.id.as_deref() == Some(id))
            .and_then(host_label)
            .unwrap_or(id);
        one_line(label)
    });
    let label = match preview {
        Some(preview) => format_primary(preview),
        None => session_label(
            session_label_source(&row.id, row.host.as_deref()),
            FALLBACK_LABEL_MAX_CHARS,
        ),
    };
    let tag_key = tag.as_deref().unwrap_or("");
    let host_key = host.as_deref().unwrap_or("");
    let mut filter_key = format!("{tag_key} {} {host_key}", row.id);
    if let Some(prompt) = preview.and_then(|preview| preview.first_user_message.as_deref()) {
        filter_key = format!("{prompt} {filter_key}");
    }
    // The directory snapshot owns archive state, not a preview read racing
    // an archive command.
    let item = SelectItem::new(label, filter_key)
        .with_columns(session_columns(row, host.as_deref(), preview, now))
        .with_strikethrough(row.archived);
    decorate_row(item, tag, row.id == current)
}

/// The row's textual state. Unreachable, locked, and working keep the
/// sidebar's priority, then this surface distinguishes a live session from a
/// cold idle one. The sidebar's remaining state is unseen output instead, so
/// its [`crate::sidebar::RowStatus`] is not this formatter's vocabulary.
fn session_state(row: &SessionSummary) -> &'static str {
    if row.unreachable {
        "unreachable"
    } else if row.locked {
        "in use"
    } else if row.working {
        "working"
    } else if row.live {
        "live"
    } else {
        "idle"
    }
}

fn session_columns(
    row: &SessionSummary,
    host: Option<&str>,
    preview: Option<&SessionPreview>,
    now: DateTime<Utc>,
) -> Vec<SelectColumn> {
    // Empty host fields reserve no space when every row is local or direct,
    // but keep metadata aligned if a directory row has no host label.
    let mut columns = vec![
        // A one-cell field survives metadata clipping independently of the
        // explanatory state text. Focus keeps its own marker gutter.
        SelectColumn::new(if row.locked && !row.unreachable {
            LOCKED_MARKER
        } else {
            ""
        })
        .with_gap_after(1),
        SelectColumn::new(host.unwrap_or("")),
        SelectColumn::new(session_state(row)),
    ];
    columns.extend(match preview {
        Some(preview) => log_columns(preview, now),
        None => vec![
            SelectColumn::new("").with_gap_after(1),
            SelectColumn::new(""),
            SelectColumn::new(""),
            SelectColumn::new(format!("last {}", format_age(now, row.last_activity))),
        ],
    });
    columns
}

/// Give each row its current marker and scoped tag column. Tags are folded to
/// one line before display and filtering so stored control characters cannot
/// reach the terminal, and the scoped query matches the displayed text.
fn decorate_row(item: SelectItem, tag: Option<String>, is_current: bool) -> SelectItem {
    let mut item = item;
    if is_current {
        item = item.with_marker(FOCUS_MARKER.chars().next().expect("focus marker"));
    }
    match tag {
        Some(tag) => item
            .with_prefix(truncate_chars(&tag, TAG_COLUMN_MAX_CHARS))
            .with_scope_key(tag),
        None => item,
    }
}

/// The footer. Enter and Esc are the widget's built-in keys (not rebindable
/// actions), so they keep the fixed convention. Only the labels resolve
/// through the keybinding data.
///
/// The toggle hint names what the chord would show, not the state the list is
/// in, so the footer reads as the offer it is.
fn subtitle(reveal: bool) -> String {
    let confirm = confirm_key_label();
    let close = close_key_label();
    let toggle = action_shortcut(ACTION_SESSION_TOGGLE_ARCHIVED)
        .expect("aj.session.toggle_archived has a default chord");
    let offer = if reveal { "hide" } else { "show" };
    format!("{confirm} to resume  \u{2022}  {toggle} {offer} archived  \u{2022}  {close} to close")
}

/// The trailing preview shows the first prompt line. The row widget clips it
/// to the space left after metadata. Search retains the entire prompt.
fn format_primary(preview: &SessionPreview) -> String {
    let raw = preview
        .first_user_message
        .as_deref()
        .unwrap_or("(no user message yet)");
    one_line(raw.lines().next().unwrap_or(raw))
}

/// Inline labels stay beside their values. Counts and units occupy separate
/// cells so the numbers align even when the unit is singular.
fn log_columns(preview: &SessionPreview, now: DateTime<Utc>) -> Vec<SelectColumn> {
    vec![
        SelectColumn::new(preview.message_count.to_string())
            .with_alignment(TextAlign::Right)
            .with_gap_after(1),
        SelectColumn::new(if preview.message_count == 1 {
            "msg"
        } else {
            "msgs"
        }),
        SelectColumn::new(format!(
            "created {}",
            format_created(now, preview.created_at)
        )),
        SelectColumn::new(format!("last {}", format_age(now, preview.last_message_at))),
    ]
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
    use aj_wire::QueueCounts;
    use chrono::Duration;

    use super::*;

    #[test]
    fn locked_rows_stay_recognizable_and_selectable_when_metadata_is_clipped() {
        use vaxis::key::Key;
        use vaxis::vxfw::Phase;

        for (locked, unreachable) in [(true, false), (false, false), (true, true)] {
            for host in [None, Some("long-host-name")] {
                let handles = OverlayHandles::for_tests();
                let rows = [
                    SessionSummary {
                        locked,
                        unreachable,
                        ..directory_row(
                            "current",
                            Some("long-session-tag"),
                            host,
                            Duration::hours(2),
                        )
                    },
                    directory_row("other", None, host, Duration::hours(1)),
                ];
                open_session_selector(&handles, "current".to_string(), &rows, &[]);
                let scan = handles.session_scan.borrow_mut().take().expect("scan");
                let window = Rc::clone(&handles.stack.borrow().top().expect("open").widget);
                for previews in [false, true] {
                    if previews {
                        extend_session_scan(
                            &scan,
                            &[
                                preview(
                                    "current",
                                    Some("recognize this session and its long prompt"),
                                    123456,
                                    Duration::hours(2),
                                ),
                                preview(
                                    "other",
                                    Some("another session prompt"),
                                    1,
                                    Duration::hours(1),
                                ),
                            ],
                            Utc::now(),
                        );
                    }
                    for width in [160, 80, 60, 48, 40, 160] {
                        let (_, size) = OverlayPlacement::Large
                            .resolve(vaxis::vxfw::Size { width, height: 24 });
                        let surface = window.borrow_mut().draw(&crate::test_support::draw_ctx(
                            size.width,
                            Some(size.height),
                        ));
                        let lines = crate::test_support::rows(&surface);
                        let current = lines
                            .iter()
                            .find(|line| line.contains(FOCUS_MARKER))
                            .expect("focus remains visible");
                        assert_eq!(
                            current.contains("L"),
                            locked && !unreachable,
                            "width={width}: {current}"
                        );
                        assert_eq!(
                            lines.iter().filter(|line| line.contains("L")).count(),
                            usize::from(locked && !unreachable),
                            "only the locked row is marked: {lines:?}"
                        );
                        if previews {
                            assert!(current.contains("recognize"), "width={width}: {current}");
                        }
                        if width == 160 {
                            let state = if unreachable {
                                "unreachable"
                            } else if locked {
                                "in use"
                            } else {
                                "idle"
                            };
                            assert!(current.contains(state), "{current}");
                        }
                    }
                }
                let mut ctx = EventContext::new();
                ctx.phase = Phase::Capturing;
                scan.select.borrow_mut().capture_event(
                    &mut ctx,
                    &Event::KeyPress(Key {
                        codepoint: Key::ENTER,
                        ..Key::default()
                    }),
                );
                assert!(
                    matches!(handles.session_request.borrow().as_ref(), Some(SessionRequest::Resume(id)) if id == "current")
                );
                assert!(!handles.stack.borrow().is_open());
            }
        }
    }

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
            archived: false,
        }
    }

    fn directory_row(
        id: &str,
        tag: Option<&str>,
        host: Option<&str>,
        age: Duration,
    ) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            live: false,
            working: false,
            queued: QueueCounts::default(),
            tasks: 0,
            last_seq: None,
            last_activity: Utc::now() - age,
            tag: tag.map(str::to_string),
            host: host.map(str::to_string),
            unreachable: false,
            archived: false,
            locked: false,
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

    /// Matching directory and preview fixtures, without reading a session store.
    fn snapshot(previews: &[SessionPreview]) -> Vec<SessionSummary> {
        previews
            .iter()
            .map(|preview| SessionSummary {
                archived: preview.archived,
                last_activity: preview.last_message_at,
                ..directory_row(
                    &preview.session_id,
                    preview.tag.as_deref(),
                    None,
                    Duration::zero(),
                )
            })
            .collect()
    }

    fn preview_item(preview: &SessionPreview, is_current: bool, now: DateTime<Utc>) -> SelectItem {
        let rows = snapshot(std::slice::from_ref(preview));
        let current = if is_current {
            preview.session_id.as_str()
        } else {
            ""
        };
        build_item(&rows[0], &[], Some(preview), current, now)
    }

    fn scan_over(previews: Vec<SessionPreview>, current: &str) -> (SessionScan, OverlayHandles) {
        let handles = OverlayHandles::for_tests();
        open_session_selector(&handles, current.to_string(), &snapshot(&previews), &[]);
        let scan = handles
            .session_scan
            .borrow_mut()
            .take()
            .expect("open parked a scan");
        extend_session_scan(&scan, &previews, Utc::now());
        (scan, handles)
    }

    #[test]
    fn build_item_tags_the_current_row_and_indexes_both_prompt_and_id() {
        let p = preview(
            "2025-05-09",
            Some("debug the streaming protocol"),
            17,
            Duration::hours(3),
        );
        let current = preview_item(&tagged(p.clone(), "fix-auth"), true, Utc::now());
        assert!(current.label.contains("debug the streaming protocol"));
        assert_eq!(current.marker, Some('▌'));
        // The filter key carries the prompt, the label, and the id.
        assert!(current.filter_key.contains("debug the streaming protocol"));
        assert!(current.filter_key.contains("fix-auth"));
        assert!(current.filter_key.contains("2025-05-09"));

        let other = preview_item(&p, false, Utc::now());
        assert_eq!(other.marker, None);
        assert!(
            !other.filter_key.contains("fix-auth"),
            "an untagged row indexes no label: {}",
            other.filter_key,
        );
    }

    fn metadata_text(item: &SelectItem) -> String {
        item.columns
            .iter()
            .map(|column| column.text.as_str())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
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
        let item = preview_item(&tagged(p.clone(), "fix-auth"), false, Utc::now());
        assert_eq!(item.prefix.as_deref(), Some("fix-auth"));
        assert_eq!(item.label, "debug the protocol");
        assert_eq!(item.scope_key.as_deref(), Some("fix-auth"));
        assert!(metadata_text(&item).starts_with("idle 42 msgs created "));

        let untagged = preview_item(&p, false, Utc::now());
        assert_eq!(untagged.prefix, None);
        assert_eq!(untagged.scope_key, None);
        assert_eq!(metadata_text(&untagged), metadata_text(&item));
    }

    #[test]
    fn a_directory_row_uses_the_directory_label_host_and_state() {
        let now = Utc::now();
        let mut row = directory_row(
            "host-a:2025-05-09-14-30-00-000",
            Some("fix-auth"),
            Some("host-a"),
            Duration::hours(2),
        );
        row.last_activity = now - Duration::hours(2);
        row.live = true;
        row.working = true;
        let hosts = vec![DirectoryHost {
            id: Some("host-a".to_string()),
            address: None,
            name: Some("Studio Left".to_string()),
            working_directory: None,
            unreachable: false,
        }];

        let item = build_item(&row, &hosts, None, &row.id, now);

        assert_eq!(item.prefix.as_deref(), Some("fix-auth"));
        assert_eq!(item.scope_key.as_deref(), Some("fix-auth"));
        assert_eq!(item.label, "14-30-00");
        assert_eq!(item.marker, Some('▌'));
        assert_eq!(metadata_text(&item), "Studio Left working last 2h",);
        assert!(item.filter_key.contains("fix-auth"));
        assert!(item.filter_key.contains("host-a:2025-05-09-14-30-00-000"));
        assert!(item.filter_key.contains("Studio Left"));
    }

    #[test]
    fn session_state_precedence_keeps_live_distinct_from_idle() {
        let base = directory_row("session", None, None, Duration::minutes(1));
        for (row, expected) in [
            (
                SessionSummary {
                    unreachable: true,
                    working: true,
                    live: true,
                    ..base.clone()
                },
                "unreachable",
            ),
            (
                SessionSummary {
                    working: true,
                    live: true,
                    ..base.clone()
                },
                "working",
            ),
            (
                SessionSummary {
                    live: true,
                    ..base.clone()
                },
                "live",
            ),
            (base, "idle"),
        ] {
            assert_eq!(session_state(&row), expected);
        }
    }

    #[test]
    fn archived_rows_obey_reveal_and_current_exemption() {
        let now = Utc::now();
        let mut current = directory_row("current", None, None, Duration::minutes(1));
        current.archived = true;
        current.last_activity = now - Duration::minutes(1);
        let mut other = directory_row("other", None, None, Duration::minutes(2));
        other.archived = true;
        other.last_activity = now - Duration::minutes(2);
        let ids = Rc::new(RefCell::new(HashMap::new()));
        let rows = SelectorRows {
            rows: vec![current, other],
            hosts: Vec::new(),
            previews: Rc::new(RefCell::new(Vec::new())),
        };

        let hidden = rows.items(&ids, "current", false, now);
        assert_eq!(hidden.len(), 1);
        assert_eq!(hidden[0].label, "current");
        assert_eq!(hidden[0].marker, Some('▌'));
        assert_eq!(
            metadata_text(&hidden[0]),
            "idle last 1m",
            "a plain-host row omits the host part",
        );

        let shown = rows.items(&ids, "current", true, now);
        assert_eq!(shown.len(), 2);
        assert!(
            shown
                .iter()
                .all(|item| metadata_text(item).starts_with("idle last "))
        );
    }

    #[test]
    fn a_snapshot_preselects_before_previews_arrive() {
        let handles = OverlayHandles::for_tests();
        open_session_selector(
            &handles,
            "current".to_string(),
            &[
                directory_row("other", None, None, Duration::minutes(1)),
                directory_row("current", None, None, Duration::minutes(2)),
            ],
            &[],
        );
        let scan = handles
            .session_scan
            .borrow_mut()
            .take()
            .expect("preview scan parked");
        let selected = scan
            .select
            .borrow()
            .selected()
            .map(|item| item.filter_key)
            .expect("current selected");
        assert_eq!(
            scan.ids.borrow().get(&selected).map(String::as_str),
            Some("current")
        );
        assert!(scan.is_open(&handles.stack.borrow()));
        extend_session_scan(
            &scan,
            &[preview(
                "current",
                Some("current prompt"),
                2,
                Duration::minutes(2),
            )],
            Utc::now(),
        );
        let selected = scan
            .select
            .borrow()
            .selected()
            .map(|item| item.filter_key)
            .expect("current still selected");
        assert_eq!(
            scan.ids.borrow().get(&selected).map(String::as_str),
            Some("current")
        );
        assert!(selected.contains("current prompt"));
        assert_eq!(
            scan.select.borrow().visible_labels().len(),
            2,
            "unread rows remain usable"
        );
        scan.finish(true);
        let drawn = drawn_rows(&handles).join("\n");
        assert!(drawn.contains("preview search incomplete"), "{drawn}");
        scan.finish(false);
        assert_eq!(
            scan.window.as_ref().unwrap().borrow().title,
            "Resume session"
        );
        if let Some(cancel) = scan.select.borrow_mut().on_cancel.as_mut() {
            cancel(&mut EventContext::new());
        }
        assert!(!scan.is_open(&handles.stack.borrow()));
        assert!(handles.session_request.borrow().is_none());
    }

    /// Previews land one at a time behind an already usable list. Each arrival
    /// fills in its own row and leaves the highlight on the row the user put it
    /// on, including when that row is the one being filled in.
    #[test]
    fn arriving_previews_fill_rows_in_place_and_keep_the_highlight() {
        let handles = OverlayHandles::for_tests();
        let rows: Vec<_> = (0..40)
            .map(|i| directory_row(&format!("s{i:02}"), None, None, Duration::minutes(i)))
            .collect();
        open_session_selector(&handles, "s00".to_string(), &rows, &[]);
        let scan = handles
            .session_scan
            .borrow_mut()
            .take()
            .expect("preview scan parked");
        drawn_rows(&handles);
        assert!(
            scan.select
                .borrow()
                .select_matching(|item| item.filter_key.contains("s33"))
        );
        let before = drawn_rows(&handles);
        let anchor = before
            .iter()
            .position(|row| row.contains("s32"))
            .expect("an unchanged row is visible beside the selection");
        for i in [33i64, 5, 31] {
            extend_session_scan(
                &scan,
                &[preview(
                    &format!("s{i:02}"),
                    Some(&format!("prompt {i}")),
                    1,
                    Duration::minutes(i),
                )],
                Utc::now(),
            );
            let selected = scan
                .select
                .borrow()
                .selected()
                .map(|item| item.filter_key)
                .expect("highlight present");
            assert_eq!(
                scan.ids.borrow().get(&selected).map(String::as_str),
                Some("s33"),
                "arrival of s{i:02} moved the highlight"
            );
            assert_eq!(
                drawn_rows(&handles)
                    .iter()
                    .position(|row| row.contains("s32")),
                Some(anchor),
                "arrival of s{i:02} scrolled the viewport"
            );
        }
        assert!(
            scan.select
                .borrow()
                .selected()
                .map(|item| item.filter_key)
                .is_some_and(|key| key.contains("prompt 33"))
        );
        let labels = scan.select.borrow().visible_labels();
        assert_eq!(labels.len(), 40);
        assert_eq!(labels[5], "prompt 5");
        assert_eq!(labels[31], "prompt 31");
        assert_eq!(labels[32], "s32", "unread rows keep their fallback label");
    }

    #[test]
    fn archive_toggle_retains_previews_and_directory_authority_across_batches() {
        use vaxis::key::{Key, Modifiers};

        let handles = OverlayHandles::for_tests();
        let rows = [
            directory_row("current", Some("directory-tag"), None, Duration::minutes(1)),
            SessionSummary {
                archived: true,
                ..directory_row("hidden", None, None, Duration::hours(1))
            },
            SessionSummary {
                archived: true,
                ..directory_row("pending", None, None, Duration::hours(2))
            },
        ];
        open_session_selector(&handles, "current".to_string(), &rows, &[]);
        let scan = handles.session_scan.borrow_mut().take().expect("scan");
        let widget = Rc::clone(&scan.window.as_ref().unwrap().borrow().child);
        let toggle = || {
            let key = Key {
                codepoint: u32::from('t'),
                mods: Modifiers::CTRL,
                ..Key::default()
            };
            assert!(action_matches(&key, ACTION_SESSION_TOGGLE_ARCHIVED));
            widget
                .borrow_mut()
                .capture_event(&mut EventContext::new(), &Event::KeyPress(key));
        };
        extend_session_scan(
            &scan,
            &[
                put_away(tagged(
                    preview("current", Some("current prompt"), 1, Duration::minutes(1)),
                    "stale-tag",
                )),
                preview("hidden", Some("hidden prompt"), 2, Duration::hours(1)),
                preview(
                    "outside-snapshot",
                    Some("must not appear"),
                    1,
                    Duration::zero(),
                ),
            ],
            Utc::now(),
        );
        assert_eq!(scan.select.borrow().visible_labels(), ["current prompt"]);
        let current = scan.select.borrow().selected().expect("current selected");
        assert_eq!(current.scope_key.as_deref(), Some("directory-tag"));
        assert!(!current.strikethrough);
        assert!(!current.filter_key.contains("stale-tag"));

        toggle();
        assert_eq!(
            scan.select.borrow().visible_labels(),
            ["current prompt", "hidden prompt", "pending"]
        );
        extend_session_scan(
            &scan,
            &[preview(
                "pending",
                Some("pending prompt"),
                3,
                Duration::hours(2),
            )],
            Utc::now(),
        );
        assert_eq!(
            scan.select.borrow().visible_labels(),
            ["current prompt", "hidden prompt", "pending prompt"]
        );
        toggle();
        extend_session_scan(
            &scan,
            &[preview(
                "hidden",
                Some("enriched hidden prompt"),
                4,
                Duration::hours(1),
            )],
            Utc::now(),
        );
        assert_eq!(scan.select.borrow().visible_labels(), ["current prompt"]);
        toggle();
        assert_eq!(
            scan.select.borrow().visible_labels(),
            ["current prompt", "enriched hidden prompt", "pending prompt"]
        );
        assert_eq!(
            scan.select.borrow().selected().unwrap().filter_key,
            current.filter_key
        );
        assert!(
            scan.select
                .borrow()
                .select_matching(|item| item.label == "enriched hidden prompt")
        );
        let picked = scan.select.borrow().selected().unwrap();
        assert!(
            picked.strikethrough,
            "archive state comes from the directory"
        );
        if let Some(confirm) = scan.select.borrow_mut().on_confirm.as_mut() {
            confirm(&mut EventContext::new(), &picked);
        }
        assert!(
            matches!(handles.session_request.borrow().as_ref(), Some(SessionRequest::Resume(id)) if id == "hidden")
        );
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
        let item = preview_item(&p, false, Utc::now());
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
        let (scan, handles) = scan_over(previews, "2025-05-09");
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
            matches!(handles.session_request.borrow().as_ref(), Some(SessionRequest::Resume(id)) if id == "2025-05-10"),
            "parked a resume for the picked id: {:?}",
            handles.session_request.borrow().as_ref().map(|_| ()),
        );
    }

    #[test]
    fn confirming_the_current_row_parks_resume_and_closes() {
        let previews = vec![preview("2025-05-09", Some("only"), 1, Duration::hours(1))];
        let (scan, handles) = scan_over(previews, "2025-05-09");
        let picked = scan.select.borrow().selected().expect("a row is selected");
        if let Some(cb) = scan.select.borrow_mut().on_confirm.as_mut() {
            let mut ctx = vaxis::vxfw::EventContext::new();
            cb(&mut ctx, &picked);
        }
        assert!(
            matches!(handles.session_request.borrow().as_ref(), Some(SessionRequest::Resume(id)) if id == "2025-05-09"),
            "the frontend decides whether the current row is a no-op or retry",
        );
        assert!(
            !handles.stack.borrow().is_open(),
            "confirm closes the overlay"
        );
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

        open_session_selector(
            &handles,
            "current".to_string(),
            &[directory_row("other", None, None, Duration::hours(1))],
            &[],
        );
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
        extend_session_scan(&scan, &previews, Utc::now());
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
    fn inline_metadata_carries_count_created_and_last() {
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
            archived: false,
        };
        assert_eq!(
            metadata_text(&preview_item(&p, false, now)),
            "idle 42 msgs created 13:22 last 2h"
        );
        assert_eq!(
            metadata_text(&preview_item(&tagged(p, "fix-auth"), false, now)),
            "idle 42 msgs created 13:22 last 2h",
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
        let item = preview_item(&p, false, Utc::now());
        assert_eq!(item.prefix.as_deref(), Some("abcd"));
        assert_eq!(item.scope_key.as_deref(), Some("abcd"));
        assert!(!item.filter_key.contains('\r'), "{}", item.filter_key);

        let mut select = FilterableSelect::new(vec![item], SelectStyles::default());
        let surface = select.draw(&crate::test_support::draw_ctx(60, Some(10)));
        assert!(!surface.children.is_empty(), "the row drew");
    }

    #[test]
    fn current_marker_uses_the_sidebar_accent_in_each_theme() {
        use crate::transcript::vaxis_color;
        use aj_app::theme::{ColorMode, Theme, ThemeColor};

        for theme in [
            Theme::bundled_dark_with_mode(ColorMode::Truecolor),
            Theme::bundled_light_with_mode(ColorMode::Truecolor),
        ] {
            let accent = vaxis_color(theme.fg_color(ThemeColor::Accent), theme.color_mode());
            let muted = vaxis_color(theme.fg_color(ThemeColor::Muted), theme.color_mode());
            assert_ne!(
                accent, muted,
                "the fixture distinguishes marker and tag colors"
            );
            let mut handles = OverlayHandles::for_tests();
            handles.chrome = crate::overlay::OverlayChrome::from_theme(&theme);
            open_session_selector(
                &handles,
                "current".to_string(),
                &[directory_row("current", None, None, Duration::hours(1))],
                &[],
            );
            let scan = handles.session_scan.borrow_mut().take().expect("scan");
            extend_session_scan(
                &scan,
                &[preview("current", Some("prompt"), 1, Duration::hours(1))],
                Utc::now(),
            );
            let window = Rc::clone(&handles.stack.borrow().top().expect("open").widget);
            let surface = window
                .borrow_mut()
                .draw(&crate::test_support::draw_ctx(100, Some(20)));
            let cells = crate::test_support::flatten(&surface);
            let marker = cells
                .iter()
                .flatten()
                .find(|cell| cell.char.grapheme() == FOCUS_MARKER)
                .expect("current marker drew");
            assert_eq!(marker.style.fg, accent);
        }
    }

    #[test]
    fn metadata_and_current_marker_survive_a_long_trailing_preview() {
        for host in [None, Some("long-lab")] {
            let handles = OverlayHandles::for_tests();
            let text = format!("{}search-tail", "preview-start ".repeat(30));
            let previews = vec![
                tagged(
                    preview("current", Some(&text), 42, Duration::hours(2)),
                    "alpha",
                ),
                tagged(
                    preview("other", Some("other prompt"), 1, Duration::hours(1)),
                    "b",
                ),
            ];
            let rows = [
                SessionSummary {
                    working: true,
                    ..directory_row("current", Some("alpha"), host, Duration::hours(2))
                },
                directory_row("other", Some("b"), host, Duration::hours(1)),
                directory_row("pending", None, host, Duration::minutes(1)),
            ];
            open_session_selector(&handles, "current".to_string(), &rows, &[]);
            let scan = handles.session_scan.borrow_mut().take().expect("scan");
            extend_session_scan(&scan, &previews, Utc::now());
            assert!(
                scan.select
                    .borrow()
                    .select_matching(|item| item.filter_key.contains("other"))
            );
            for width in [100, 120] {
                let window = Rc::clone(&handles.stack.borrow().top().expect("open").widget);
                let surface = window
                    .borrow_mut()
                    .draw(&crate::test_support::draw_ctx(width, Some(20)));
                let rows = crate::test_support::rows(&surface);
                let current = rows
                    .iter()
                    .find(|line| line.contains("alpha"))
                    .expect("current row");
                assert!(
                    current.contains('▌'),
                    "the current marker does not follow selection: {current}"
                );
                assert!(!current.contains("(current)"), "{current}");
                for metadata in ["working", "42", "msgs", "created", "last 2h"] {
                    assert!(
                        current.find(metadata).expect("metadata visible")
                            < current
                                .find("preview-start")
                                .expect("preview follows metadata"),
                        "{current}"
                    );
                }
                assert!(
                    current.contains('…') && !current.contains("search-tail"),
                    "only the preview tail clips: {current}"
                );
                let other = rows
                    .iter()
                    .find(|line| line.contains("other prompt"))
                    .expect("selected other row");
                assert!(
                    !other.contains('▌'),
                    "selection must not gain the current marker: {other}"
                );
                assert!(current.contains("42 msgs"), "{current}");
                assert!(other.contains("1 msg "), "{other}");
                assert!(!current.contains('·') && !other.contains('·'));
                let column = |line: &str, text: &str| {
                    line[..line.find(text).expect("field visible")]
                        .chars()
                        .count()
                };
                for field in ["msg", "created", "last", "preview-start"] {
                    let other_field = if field == "preview-start" {
                        "other prompt"
                    } else {
                        field
                    };
                    assert_eq!(
                        column(current, field),
                        column(other, other_field),
                        "{current}\n{other}"
                    );
                }
                assert_eq!(
                    column(current, "42") + 2,
                    column(other, "1") + 1,
                    "counts right-align"
                );
            }
        }
    }

    #[test]
    fn browser_budgets_metadata_for_prompts_on_resize_without_clipping_search() {
        use vaxis::key::Key;
        use vaxis::vxfw::{Event, EventContext, Phase};

        let long_host = format!("{}host-needle", "h".repeat(69));
        assert_eq!(long_host.len(), 80);
        let unicode_host = "界e\u{301}".repeat(13);
        for (mode, host) in [
            ("local", None),
            ("direct", None),
            ("gateway", Some("lab")),
            ("gateway", Some(long_host.as_str())),
            ("gateway", Some(unicode_host.as_str())),
        ] {
            let handles = OverlayHandles::for_tests();
            let prompt = "recognize this session and its long first line\nprompt-needle";
            let mut previews = [
                tagged(
                    preview("current", Some(prompt), 123456, Duration::hours(2)),
                    "tag",
                ),
                preview(
                    "other",
                    Some("another session prompt"),
                    1,
                    Duration::hours(1),
                ),
            ];
            previews[0].created_at = Utc::now() - Duration::days(800);
            let host_id = host.map(|_| "host-a");
            let hosts = if mode == "local" {
                Vec::new()
            } else {
                vec![DirectoryHost {
                    id: host_id.map(str::to_string),
                    address: Some("https://direct.example".to_string()),
                    name: Some(
                        host.unwrap_or("direct host must not be a column")
                            .to_string(),
                    ),
                    working_directory: None,
                    unreachable: false,
                }]
            };
            let rows = [
                SessionSummary {
                    unreachable: true,
                    ..directory_row("current", Some("tag"), host_id, Duration::hours(2))
                },
                directory_row("other", None, host.map(|_| "host-b"), Duration::hours(1)),
            ];
            open_session_selector(&handles, "current".to_string(), &rows, &hosts);
            let scan = handles.session_scan.borrow_mut().take().expect("scan");
            extend_session_scan(&scan, &previews, Utc::now());
            let window = Rc::clone(&handles.stack.borrow().top().expect("open").widget);
            let draw = |width| {
                let (_, size) =
                    OverlayPlacement::Large.resolve(vaxis::vxfw::Size { width, height: 24 });
                let surface = window.borrow_mut().draw(&crate::test_support::draw_ctx(
                    size.width,
                    Some(size.height),
                ));
                crate::test_support::flatten(&surface)
            };
            let mut wide = None;
            for width in [160, 100, 80, 64, 48, 160] {
                let cells = draw(width);
                let row = cells
                    .iter()
                    .find(|row| row.iter().any(|cell| cell.char.grapheme() == "▌"))
                    .expect("the current row draws");
                let text: String = row.iter().map(|cell| cell.char.grapheme()).collect();
                assert!(
                    text.contains("recognize this"),
                    "mode={mode}, host={host:?}, width={width}: {text}"
                );
                assert!(
                    !text.contains("direct host"),
                    "direct connections need no host column: {text}"
                );
                let metadata = &text[..text.find("recognize").unwrap()];
                // At very narrow widths even a short tag shares the clipping
                // budget. Metadata still precedes a recognizable prompt.
                if width == 48 {
                    assert!(
                        metadata
                            .split('▌')
                            .nth(1)
                            .unwrap()
                            .trim_start()
                            .starts_with('t'),
                        "tag remains identifiable: {text}"
                    );
                } else {
                    assert!(metadata.contains("tag"), "{text}");
                }
                assert!(
                    !text.contains("prompt-needle"),
                    "search tail is not rendered"
                );
                // The host needs four cells to show both graphemes and an
                // ellipsis. Narrower layouts may clip it to the wide glyph.
                if host.is_some_and(|host| host.starts_with('界')) && width >= 80 {
                    assert!(
                        row.iter()
                            .any(|cell| cell.char.grapheme() == "界" && cell.char.width == 2),
                        "{text}"
                    );
                    assert!(
                        row.iter()
                            .any(|cell| cell.char.grapheme() == "e\u{301}" && cell.char.width == 1),
                        "{text}"
                    );
                }
                if width == 160 {
                    for field in ["unreachable", "123456 msgs", "created", "last 2h"] {
                        assert!(metadata.contains(field), "{text}");
                    }
                    if let Some(wide) = &wide {
                        assert_eq!(&text, wide, "expanding restores the original layout");
                    } else {
                        wide = Some(text);
                    }
                }
            }

            // Query the clipped host suffix and the undisplayed second prompt
            // line through the actual overlay's focus, not a shortened row key.
            let queries = if host == Some(long_host.as_str()) {
                vec!["host-needle", "prompt-needle"]
            } else {
                vec!["prompt-needle"]
            };
            let focus = Rc::clone(&handles.stack.borrow().top().expect("open").focus);
            for query in queries {
                let query_len = scan.select.borrow().query().chars().count();
                for _ in 0..query_len {
                    let mut ctx = EventContext::new();
                    ctx.phase = Phase::AtTarget;
                    focus.borrow_mut().handle_event(
                        &mut ctx,
                        &Event::KeyPress(Key {
                            codepoint: Key::BACKSPACE,
                            ..Key::default()
                        }),
                    );
                }
                for c in query.chars() {
                    let mut ctx = EventContext::new();
                    ctx.phase = Phase::AtTarget;
                    focus.borrow_mut().handle_event(
                        &mut ctx,
                        &Event::KeyPress(Key {
                            codepoint: u32::from(c),
                            text: Some(c.to_string().into()),
                            ..Key::default()
                        }),
                    );
                }
                assert_eq!(scan.select.borrow().visible_labels().len(), 1);
                let text: String = draw(64)
                    .iter()
                    .flatten()
                    .map(|cell| cell.char.grapheme())
                    .collect();
                assert!(text.contains("recognize this"), "{text}");
                assert!(!text.contains("another session"), "{text}");
            }
        }
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
        open_session_selector(&handles, "current".to_string(), &snapshot(previews), &[]);
        let scan = handles
            .session_scan
            .borrow_mut()
            .take()
            .expect("open parked a scan");
        extend_session_scan(&scan, previews, Utc::now());

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

    #[test]
    fn preview_updates_refilter_the_full_prompt_and_keep_the_selected_identity() {
        let (handles, scan) = selector_over(
            &[
                preview("first", Some("not a match"), 1, Duration::minutes(1)),
                preview("second", Some("needle selected"), 1, Duration::hours(1)),
            ],
            "needle",
        );
        assert_eq!(scan.select.borrow().visible_labels(), ["needle selected"]);
        extend_session_scan(
            &scan,
            &[
                preview("first", Some("needle"), 2, Duration::minutes(1)),
                preview(
                    "second",
                    Some("recognizable prompt\nneedle"),
                    2,
                    Duration::hours(1),
                ),
            ],
            Utc::now(),
        );
        assert_eq!(scan.select.borrow().query(), "needle");
        let labels = scan.select.borrow().visible_labels();
        assert_eq!(labels.len(), 2);
        assert!(labels.iter().any(|label| label == "recognizable prompt"));
        let picked = scan
            .select
            .borrow()
            .selected()
            .expect("selection survives enrichment");
        assert_eq!(
            scan.ids
                .borrow()
                .get(&picked.filter_key)
                .map(String::as_str),
            Some("second")
        );
        if let Some(confirm) = scan.select.borrow_mut().on_confirm.as_mut() {
            confirm(&mut EventContext::new(), &picked);
        }
        assert!(
            matches!(handles.session_request.borrow().as_ref(), Some(SessionRequest::Resume(id)) if id == "second")
        );
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

    #[test]
    fn prose_search_requires_literal_terms_and_supports_quoted_phrases() {
        let previews = vec![
            preview(
                "scattered",
                Some("pondering something I was: here's"),
                1,
                Duration::minutes(1),
            ),
            preview(
                "gapped",
                Some("here's something I was p-o-n-d-e-r-i-n-g"),
                1,
                Duration::minutes(2),
            ),
            preview(
                "phrase",
                Some("Here's something I was pondering"),
                1,
                Duration::minutes(3),
            ),
        ];
        assert_eq!(
            matched(&previews, "here's something I was pondering"),
            [
                "Here's something I was pondering",
                "pondering something I was: here's"
            ],
        );
        for query in [
            "\"here's something I was pondering",
            "\"here's something I was pondering\"",
        ] {
            assert_eq!(
                matched(&previews, query),
                ["Here's something I was pondering"]
            );
        }
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
            row.split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("3 msgs created "),
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

    /// An archived session for the selector's fixtures.
    fn put_away(preview: SessionPreview) -> SessionPreview {
        SessionPreview {
            archived: true,
            ..preview
        }
    }

    /// The rows for `previews`, as the overlay builds them.
    fn rows_for(previews: &[SessionPreview], current: &str, reveal: bool) -> Vec<String> {
        let ids = Rc::new(RefCell::new(HashMap::new()));
        build_items(
            &ids,
            &snapshot(previews),
            &[],
            previews,
            current,
            reveal,
            Utc::now(),
        )
        .into_iter()
        .map(|item| item.label)
        .collect()
    }

    /// Archived sessions are out of the list until the toggle asks for them,
    /// and a revealed one comes back where its date puts it rather than into a
    /// group of its own.
    #[test]
    fn archived_previews_leave_the_list_until_the_toggle_asks() {
        let previews = vec![
            preview("2025-05-10", Some("still working"), 1, Duration::minutes(1)),
            put_away(preview(
                "2025-05-09",
                Some("done with"),
                1,
                Duration::hours(1),
            )),
            preview("2025-05-08", Some("also working"), 1, Duration::hours(2)),
        ];
        assert_eq!(
            rows_for(&previews, "none", false),
            vec!["still working", "also working"],
            "an archived session is listed before anything asked for it",
        );
        assert_eq!(
            rows_for(&previews, "none", true),
            vec!["still working", "done with", "also working"],
            "the reveal dropped a row, or moved one out of its place",
        );
    }

    /// The session the user is in is listed whatever its bit says, which is
    /// the sidebar's exemption for the row it draws as focused: archiving the
    /// session you are working in leaves it in front of you.
    #[test]
    fn the_current_session_is_listed_though_archived() {
        let previews = vec![
            put_away(preview(
                "current",
                Some("what I am in"),
                1,
                Duration::minutes(1),
            )),
            put_away(preview(
                "2025-05-09",
                Some("done with"),
                1,
                Duration::hours(1),
            )),
        ];
        assert_eq!(
            rows_for(&previews, "current", false),
            vec!["what I am in"],
            "the session on screen dropped out from under the user",
        );
    }

    #[test]
    fn archived_rows_strike_all_text_without_changing_layout_or_colors() {
        let now = Utc::now();
        let text = format!("saved prompt {}", "x".repeat(200));
        let base = tagged(
            preview("session", Some(&text), 42, Duration::hours(1)),
            "kept",
        );
        for host in [None, Some("lab")] {
            let draw = |archived: bool| {
                let handles = OverlayHandles::for_tests();
                let mut preview = base.clone();
                let mut row = directory_row("session", Some("kept"), host, Duration::hours(1));
                row.archived = archived;
                // Preview reads may race archive changes. The directory's
                // bit remains authoritative for every browser.
                preview.archived = !archived;
                open_session_selector(&handles, "session".to_string(), &[row], &[]);
                let scan = handles.session_scan.borrow_mut().take().expect("scan");
                extend_session_scan(&scan, &[preview], now);
                let window = Rc::clone(&handles.stack.borrow().top().expect("open").widget);
                let surface = window
                    .borrow_mut()
                    .draw(&crate::test_support::draw_ctx(100, Some(20)));
                crate::test_support::flatten(&surface)
                    .into_iter()
                    .find(|row| {
                        row.iter()
                            .map(|cell| cell.char.grapheme())
                            .collect::<String>()
                            .contains("saved prompt")
                    })
                    .expect("session row drew")
            };
            let plain = draw(false);
            let archived = draw(true);
            assert_eq!(plain.len(), archived.len());
            let text: String = archived.iter().map(|cell| cell.char.grapheme()).collect();
            assert!(
                text.contains('▌') && text.contains('…'),
                "marker and clipped preview are exercised: {text}"
            );
            assert!(!text.contains("archived"), "no archive column: {text}");
            for (plain, archived) in plain.iter().zip(&archived) {
                assert_eq!(
                    plain.char, archived.char,
                    "archive state cannot move columns"
                );
                assert_eq!(plain.style.fg, archived.style.fg);
                assert_eq!(plain.style.bg, archived.style.bg);
                let glyph = archived.char.grapheme();
                if !glyph.trim().is_empty() && glyph != "│" {
                    assert!(!plain.style.strikethrough, "normal row: {glyph}");
                    assert!(archived.style.strikethrough, "archived row: {glyph}");
                }
            }
        }
    }

    /// The footer says the chord and what it offers, so the toggle is
    /// discoverable from the overlay rather than from the keybinding list.
    #[test]
    fn the_footer_offers_the_archived_toggle_both_ways() {
        let hidden = subtitle(false);
        let shown = subtitle(true);
        let chord = action_shortcut(ACTION_SESSION_TOGGLE_ARCHIVED).expect("a default chord");
        assert!(hidden.contains(&chord), "{hidden}");
        assert!(
            hidden.contains("show archived"),
            "the footer offers no way to see archived sessions: {hidden}",
        );
        assert!(
            shown.contains("hide archived"),
            "the footer still offers to show what it is showing: {shown}",
        );
    }

    /// The confirm/close subtitle resolves its key labels from the
    /// keybinding data, so a rebind moves both the rendered hint and the
    /// assertion together rather than tracking a literal.
    #[test]
    fn subtitle_resolves_confirm_and_close_labels() {
        let s = subtitle(false);
        assert!(s.contains(&confirm_key_label()), "{s}");
        assert!(s.contains(&close_key_label()), "{s}");
    }
}
