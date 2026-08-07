//! The session sidebar: a strip listing every session the peer offers
//! (spec 9.2).
//!
//! The widget is read-only chrome. It renders from a [`SidebarState`] mirror
//! the drive loop refreshes once per iteration from the client's session
//! directory, the same single-writer arrangement the status chrome uses. The
//! widget cannot reach the directory itself (that lives behind the world), and
//! mirroring keeps the draw path free of any borrow the loop also wants.
//!
//! The strip is an orientation instrument, not a store browser. That is what
//! decides its behavior: rows order by last activity so the sessions in play
//! sort to the top, the drawn window follows the focused row so focus is never
//! off screen, and the stepping chords walk the order the user can see. The
//! session selector is the browser.

use std::cell::RefCell;
use std::rc::Rc;

use aj_wire::SessionSummary;
use vaxis::gwidth::{Method, gwidth};
use vaxis::vxfw::{DrawContext, MaxSize, RichText, Size, Surface, TextSpan, Widget};

use crate::transcript::TranscriptStyles;

/// Columns the sidebar occupies when shown, glyph and padding included.
///
/// Fixed rather than proportional: a strip that grew with the terminal would
/// take width from the transcript for nothing. Sized for a time-of-day label
/// plus the glyph and its separating space, with room for a slightly longer
/// hand-named session.
pub(crate) const SIDEBAR_COLS: u16 = 14;

/// Terminal width below which the strip holds itself back.
///
/// The strip is inflexible, so under this it would leave the transcript beside
/// it too little to read. Set so the transcript keeps at least a short line's
/// worth of columns.
pub(crate) const MIN_COLS_WITH_SIDEBAR: u16 = SIDEBAR_COLS + 20;

/// Columns a label may occupy: everything but the glyph and its space.
fn label_cols(width: u16) -> usize {
    usize::from(width.saturating_sub(2))
}

/// What a row's glyph says about its session, in the order a row that could
/// claim several of these should claim one (spec 6.8, 9.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowStatus {
    /// The peer cannot reach the host this session lives on.
    Unreachable,
    /// A turn is running.
    Working,
    /// Idle, and it has produced output since the user last looked at it.
    Unseen,
    Idle,
}

impl RowStatus {
    /// What `row` should show, given whether the client derived unseen output
    /// for it.
    ///
    /// The order is the precedence: unreachable first because it says the peer
    /// cannot answer for this session at all, so `working` and the activity
    /// stamp behind `unseen` are both stale rather than wrong. Working next
    /// because it is the live fact. Unseen is what remains once a session
    /// stops, which is why it cannot outrank working (spec 6.8).
    pub(crate) fn of(row: &SessionSummary, unseen: bool) -> Self {
        if row.unreachable {
            RowStatus::Unreachable
        } else if row.working {
            RowStatus::Working
        } else if unseen {
            RowStatus::Unseen
        } else {
            RowStatus::Idle
        }
    }

    /// The glyph for this status.
    ///
    /// Idle is blank on purpose: most rows are idle most of the time, and a
    /// glyph on every row would leave the interesting ones no way to stand out.
    fn glyph(self) -> &'static str {
        match self {
            RowStatus::Unreachable => "!",
            RowStatus::Working => "*",
            RowStatus::Unseen => "•",
            RowStatus::Idle => " ",
        }
    }
}

/// One row of the sidebar.
pub(crate) struct SidebarRow {
    pub(crate) id: String,
    pub(crate) status: RowStatus,
    /// Whether this is the session on screen.
    pub(crate) focused: bool,
}

/// What the sidebar draws, mirrored from the session directory once per drive
/// loop iteration.
#[derive(Default)]
pub(crate) struct SidebarState {
    /// Whether the strip is wanted, by default or by an explicit ask.
    pub(crate) visible: bool,
    /// Whether the user has worked the toggle, which pins `visible` against the
    /// default that otherwise follows the row count (spec 9.2).
    pub(crate) toggled: bool,
    /// Whether the terminal is too narrow to spare the columns, resolved per
    /// frame by the shell, which is the only place the width is known.
    pub(crate) too_narrow: bool,
    /// Rows in display order, most recent activity first.
    pub(crate) rows: Vec<SidebarRow>,
}

impl SidebarState {
    /// Whether the strip is drawn: wanted, and the terminal can spare it.
    pub(crate) fn shown(&self) -> bool {
        self.visible && !self.too_narrow
    }
}

/// Build the display rows from the peer's directory.
///
/// Ordered by last activity, newest first, with ties broken by id descending so
/// the order is total: an unstable order would reshuffle under a user stepping
/// through it. `unseen` answers spec 6.8's "has it moved since I looked" for a
/// row the caller already holds, which keeps this linear.
pub(crate) fn rows_for_display(
    rows: &[SessionSummary],
    focused: &str,
    unseen: impl Fn(&SessionSummary) -> bool,
) -> Vec<SidebarRow> {
    let mut ordered: Vec<&SessionSummary> = rows.iter().collect();
    ordered.sort_by(|l, r| {
        r.last_activity
            .cmp(&l.last_activity)
            .then_with(|| r.id.cmp(&l.id))
    });
    ordered
        .into_iter()
        .map(|row| SidebarRow {
            status: RowStatus::of(row, unseen(row)),
            focused: row.id == focused,
            id: row.id.clone(),
        })
        .collect()
}

/// The session a next/previous step lands on, walking the displayed order and
/// wrapping at the ends.
///
/// `None` when there is nothing to move to: fewer than two rows, or no row
/// claiming focus (the directory and the rows disagree, so any answer would be
/// a guess).
pub(crate) fn step_session(state: &SidebarState, forward: bool) -> Option<String> {
    let len = state.rows.len();
    if len < 2 {
        return None;
    }
    let at = state.rows.iter().position(|row| row.focused)?;
    let next = if forward {
        (at + 1) % len
    } else {
        (at + len - 1) % len
    };
    Some(state.rows[next].id.clone())
}

/// The contiguous run of rows to draw, chosen so the focused row is inside it.
///
/// Scrolls by the least it can: the window only moves once focus would fall off
/// its bottom edge, which keeps a step from jumping the whole strip. With no
/// focused row it shows the top, which is the most recently active end.
fn window(rows: &[SidebarRow], height: usize) -> &[SidebarRow] {
    if height == 0 || rows.len() <= height {
        return rows;
    }
    let focused = rows.iter().position(|row| row.focused).unwrap_or(0);
    let start = focused.saturating_sub(height - 1);
    &rows[start..start + height]
}

/// The sidebar strip.
pub(crate) struct SessionSidebar {
    state: Rc<RefCell<SidebarState>>,
    styles: Rc<TranscriptStyles>,
}

impl SessionSidebar {
    pub(crate) fn new(state: Rc<RefCell<SidebarState>>, styles: Rc<TranscriptStyles>) -> Self {
        Self { state, styles }
    }

    /// The visible part of a session id: its time of day.
    ///
    /// Minted ids are `YYYY-MM-DD-HH-MM-SS-mmm`, and the date is the same for
    /// every session the user is likely to be juggling, so the leading date
    /// earns none of the width it costs. An id in any other shape (a
    /// hand-renamed file) is shown from its tail, where what distinguishes such
    /// a name usually is.
    ///
    /// `width` is a budget in display columns, not characters, so a label of
    /// wide graphemes cannot overflow its column and wrap. Wrapping would break
    /// the strip's one-line-per-row correspondence and misattribute every row
    /// below it. Control characters are dropped for the same reason: a session
    /// file may be named anything a filesystem accepts.
    ///
    /// NOTE: two sessions minted in the same second share a label. The strip is
    /// for orientation, not identification, and the header carries the focused
    /// session's id in full.
    pub(crate) fn label(id: &str, width: usize) -> String {
        let cleaned: String = id.chars().filter(|c| !c.is_control()).collect();
        let parts: Vec<&str> = cleaned.split('-').collect();
        // The minted shape, not merely something with seven dashes: a
        // hand-named session can have as many, and slicing its middle out would
        // show a fragment of a word.
        let minted = parts.len() == 7
            && parts[0].len() == 4
            && parts[..6]
                .iter()
                .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
        let label = if minted {
            parts[3..6].join("-")
        } else {
            cleaned
        };
        truncate_to_cols(&label, width)
    }
}

/// Trim `label` from its head until it fits `cols` display columns, which keeps
/// the tail where a hand-named session's distinguishing part is.
fn truncate_to_cols(label: &str, cols: usize) -> String {
    let Ok(budget) = u16::try_from(cols) else {
        return label.to_string();
    };
    if gwidth(label, Method::Unicode) <= budget {
        return label.to_string();
    }
    let mut out = String::new();
    // Build from the tail so the trailing characters survive, then reverse.
    for ch in label.chars().rev() {
        let mut candidate = String::with_capacity(out.len() + 4);
        candidate.push(ch);
        candidate.push_str(&out);
        if gwidth(&candidate, Method::Unicode) > budget {
            break;
        }
        out = candidate;
    }
    out
}

impl Widget for SessionSidebar {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let state = self.state.borrow();
        if !state.shown() {
            return Surface::with_size(Size {
                width: 0,
                height: 0,
            });
        }
        // A flex row measures its inflexible children under an unbounded width,
        // so the width has to come from this widget rather than the context:
        // that measurement is exactly the question "how wide are you".
        let width = ctx
            .max
            .width
            .map_or(SIDEBAR_COLS, |max| max.min(SIDEBAR_COLS));
        if width == 0 {
            return Surface::with_size(Size {
                width: 0,
                height: 0,
            });
        }
        let label_width = label_cols(width);
        let drawn = match ctx.max.height {
            Some(height) => window(&state.rows, usize::from(height)),
            None => &state.rows,
        };
        let mut spans: Vec<TextSpan> = Vec::with_capacity(drawn.len() * 2);
        for (index, row) in drawn.iter().enumerate() {
            if index > 0 {
                spans.push(TextSpan {
                    text: "\n".to_string(),
                    style: self.styles.text,
                    ..TextSpan::default()
                });
            }
            let style = match (row.focused, row.status) {
                // The focused row is the accent whatever its status: the user is
                // looking at it, so what it is doing is on screen anyway.
                (true, _) => self.styles.accent,
                (false, RowStatus::Unreachable) => self.styles.error,
                (false, RowStatus::Working) => self.styles.success,
                (false, RowStatus::Unseen) => self.styles.warning,
                (false, RowStatus::Idle) => self.styles.dim,
            };
            spans.push(TextSpan {
                text: format!(
                    "{} {}",
                    row.status.glyph(),
                    Self::label(&row.id, label_width)
                ),
                style,
                ..TextSpan::default()
            });
        }
        let mut text = RichText::new(spans);
        let mut surface = text.draw(&ctx.with_constraints(
            Size { width, height: 0 },
            MaxSize {
                width: Some(width),
                height: ctx.max.height,
            },
        ));
        // The strip owns its full column width even where a row is shorter, so
        // the transcript beside it never starts mid-strip.
        surface.size.width = width;
        surface
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_shows_its_time_of_day() {
        assert_eq!(
            SessionSidebar::label("2026-08-06-19-07-19-368", 12),
            "19-07-19",
            "the date is the same for every session in a sitting, so it is dropped",
        );
    }

    /// An id in another shape (a hand-renamed session file) is shown from its
    /// tail, which is where what distinguishes such a name usually is.
    #[test]
    fn an_unfamiliar_id_is_shown_from_its_tail() {
        assert_eq!(SessionSidebar::label("short", 12), "short");
        assert_eq!(
            SessionSidebar::label("a-very-long-hand-named-session", 8),
            "-session",
        );
    }

    /// Seven dashes do not make a minted id. A hand-named session with as many
    /// components must not have its middle sliced out and shown as a label.
    #[test]
    fn a_hand_named_id_with_seven_parts_is_still_shown_from_its_tail() {
        assert_eq!(
            SessionSidebar::label("notes-on-the-rust-borrow-checker-draft", 12),
            "hecker-draft",
        );
    }

    /// A collision suffix rides the id's last component, so the id still reads
    /// as the minted shape and still shows its time of day.
    #[test]
    fn a_collision_suffix_still_reads_as_a_minted_id() {
        assert_eq!(
            SessionSidebar::label("2026-08-06-19-07-19-368_2", 12),
            "19-07-19",
        );
    }

    /// A label is budgeted in display columns, so wide graphemes cannot overflow
    /// the strip and wrap onto the next row's line.
    #[test]
    fn a_wide_character_label_fits_its_columns() {
        let label = SessionSidebar::label("会話ノート記録帳", 12);
        assert!(
            gwidth(&label, Method::Unicode) <= 12,
            "{label:?} takes {} columns",
            gwidth(&label, Method::Unicode),
        );
        assert!(
            label.chars().count() < 8,
            "and it really had to drop characters to get there: {label:?}",
        );
    }

    /// A session file can be named anything the filesystem accepts, and a
    /// newline in a label would split its own row and misattribute every row
    /// below it.
    #[test]
    fn a_control_character_cannot_split_a_row() {
        let label = SessionSidebar::label("first\nsecond", 12);
        assert!(!label.contains('\n'), "{label:?} would break the row");
    }

    fn summary(working: bool, unreachable: bool) -> SessionSummary {
        SessionSummary {
            id: "session-1".to_string(),
            live: true,
            working,
            queued: aj_wire::QueueCounts::default(),
            tasks: 0,
            last_seq: Some(0),
            last_activity: chrono::Utc::now(),
            unreachable,
        }
    }

    /// The precedence between the four statuses, which no timing-dependent test
    /// can pin: unreachable outranks everything because it says the rest of the
    /// row is stale, working outranks unseen because unseen is what remains once
    /// a session stops.
    #[test]
    fn a_rows_status_follows_its_precedence() {
        assert_eq!(
            RowStatus::of(&summary(false, false), false),
            RowStatus::Idle,
        );
        assert_eq!(
            RowStatus::of(&summary(false, false), true),
            RowStatus::Unseen,
        );
        assert_eq!(
            RowStatus::of(&summary(true, false), true),
            RowStatus::Working,
            "a running turn is the more useful fact than output not yet read",
        );
        assert_eq!(
            RowStatus::of(&summary(true, true), true),
            RowStatus::Unreachable,
            "an unreachable host makes working and unseen both stale",
        );
    }

    /// Only the interesting statuses spend a glyph. Most rows are idle most of
    /// the time, and a mark on every row would leave the others nothing to
    /// stand out against.
    #[test]
    fn only_the_interesting_statuses_wear_a_glyph() {
        assert_eq!(RowStatus::Idle.glyph(), " ");
        for status in [
            RowStatus::Working,
            RowStatus::Unseen,
            RowStatus::Unreachable,
        ] {
            assert_ne!(status.glyph(), " ", "{status:?} needs a mark");
            assert_eq!(
                status.glyph().chars().count(),
                1,
                "{status:?} fits one cell"
            );
        }
    }

    fn at(id: &str, minutes: i64) -> SessionSummary {
        SessionSummary {
            id: id.to_string(),
            last_activity: chrono::Utc::now() - chrono::Duration::minutes(minutes),
            ..summary(false, false)
        }
    }

    fn ids(rows: &[SidebarRow]) -> Vec<&str> {
        rows.iter().map(|row| row.id.as_str()).collect()
    }

    /// Rows order by activity, newest first, which is what makes the strip an
    /// orientation instrument: the sessions in play sort to the top rather than
    /// wherever their creation date puts them.
    #[test]
    fn rows_order_by_activity_not_by_id() {
        // Ids ascend while activity descends, so an id-ordered result is the
        // exact reverse and cannot pass by accident.
        let rows = vec![
            at("session-a", 30),
            at("session-b", 10),
            at("session-c", 20),
        ];
        let display = rows_for_display(&rows, "session-b", |_| false);
        assert_eq!(ids(&display), vec!["session-b", "session-c", "session-a"]);
        assert!(display[0].focused, "the focused row is marked");
        assert!(!display[1].focused);
    }

    /// Equal stamps are broken by id so the order is total. Without a tiebreak
    /// the sort would leave equal rows in input order, which reshuffles under a
    /// user stepping through them.
    #[test]
    fn rows_with_equal_activity_order_by_id() {
        let now = chrono::Utc::now();
        let mut a = at("session-a", 0);
        let mut b = at("session-b", 0);
        a.last_activity = now;
        b.last_activity = now;
        let display = rows_for_display(&[a, b], "session-a", |_| false);
        assert_eq!(ids(&display), vec!["session-b", "session-a"]);
    }

    fn row(id: &str, focused: bool) -> SidebarRow {
        SidebarRow {
            id: id.to_string(),
            status: RowStatus::Idle,
            focused,
        }
    }

    fn state_of(focused_at: usize, len: usize) -> SidebarState {
        SidebarState {
            visible: true,
            rows: (0..len)
                .map(|i| row(&format!("session-{i}"), i == focused_at))
                .collect(),
            ..SidebarState::default()
        }
    }

    /// Stepping walks the displayed order in the direction asked for. Three rows
    /// is the smallest set that can tell the directions apart: with two, next
    /// and previous land on the same row.
    #[test]
    fn stepping_walks_the_displayed_order_both_ways() {
        let state = state_of(1, 3);
        assert_eq!(step_session(&state, true).as_deref(), Some("session-2"));
        assert_eq!(step_session(&state, false).as_deref(), Some("session-0"));
    }

    /// Both ends wrap, so a step never dead-ends.
    #[test]
    fn stepping_wraps_at_both_ends() {
        assert_eq!(
            step_session(&state_of(2, 3), true).as_deref(),
            Some("session-0"),
            "forward off the bottom wraps to the top",
        );
        assert_eq!(
            step_session(&state_of(0, 3), false).as_deref(),
            Some("session-2"),
            "backward off the top wraps to the bottom",
        );
    }

    /// Nothing to step to: one row is already focused, and rows that name no
    /// focused session would make any answer a guess.
    #[test]
    fn stepping_needs_somewhere_to_go() {
        assert_eq!(step_session(&state_of(0, 1), true), None);
        assert_eq!(step_session(&SidebarState::default(), true), None);
        let unfocused = SidebarState {
            visible: true,
            rows: vec![row("session-0", false), row("session-1", false)],
            ..SidebarState::default()
        };
        assert_eq!(step_session(&unfocused, true), None);
    }

    /// The window follows focus, so the row the user is on is always drawn even
    /// when the store holds more sessions than the terminal has lines.
    #[test]
    fn the_drawn_window_keeps_the_focused_row_visible() {
        let state = state_of(7, 10);
        let drawn = window(&state.rows, 3);
        assert_eq!(ids(drawn), vec!["session-5", "session-6", "session-7"]);
        assert!(
            drawn.iter().any(|row| row.focused),
            "the focused row is inside the window",
        );
    }

    /// It scrolls by the least it can: while focus still fits above the bottom
    /// edge the window stays at the top, so a step does not jump the strip.
    #[test]
    fn the_window_holds_still_while_focus_fits() {
        let state = state_of(1, 10);
        assert_eq!(
            ids(window(&state.rows, 3)),
            vec!["session-0", "session-1", "session-2"]
        );
    }

    /// Fewer rows than lines draws them all, and a zero-height context is not a
    /// reason to panic.
    #[test]
    fn a_short_list_is_drawn_whole() {
        let state = state_of(0, 2);
        assert_eq!(window(&state.rows, 5).len(), 2);
        assert_eq!(window(&state.rows, 0).len(), 2);
    }
}
