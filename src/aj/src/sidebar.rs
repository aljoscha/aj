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
//!
//! A row answers three independent questions, and each gets exactly one
//! encoding so none of them has to be read out of a combination:
//!
//! - What is the session doing: the status glyph and its color. It says the
//!   same thing whether or not the client holds the session open.
//! - Does the client hold it open: the label's brightness, accent for the
//!   focused session, plain text for one attached in the background, dim for
//!   one the peer has merely listed. Brightness rather than a second hue,
//!   because brightness survives a monochrome or low-contrast theme.
//! - Which session is on screen: the [`FOCUS_MARKER`] in the leftmost column,
//!   so focus never rests on color alone.
//!
//! An unattached session running a turn therefore reads as a bright glyph
//! beside a dim label. That is the intended reading: something is happening
//! there and you do not have it open. An attached session on a host that has
//! gone out reads the other way round, an error glyph beside a label at
//! attached brightness, and that is the intended reading too: you have it
//! open and its host cannot be reached.
//!
//! The label field itself is two columns. The id-derived label leads (a
//! minted id's time of day), and the tag the user gave the session
//! supplements it to the right (see [`SidebarRow::label`]).
//!
//! Layout is a pure function, [`strip_lines`], producing one [`StripLine`] per
//! drawn line, and drawing is a dumb map over its result. The height
//! arithmetic (host headers, the overflow row and the create row all take
//! lines away from the rows) therefore lives in one testable place.
//!
//! Pointer gestures are a second trigger for actions the chords already
//! dispatch, never a behavior of their own (spec 9.2). The strip resolves a
//! click into a [`StripGesture`], which names a session or a create and
//! nothing else, and the shell hands that to the same place the chord's
//! handler hands its own answer. A draw records what each line it paints
//! resolves to, so a click answers with the session the user is looking at
//! even when the mirror has moved since (see [`SessionSidebar::gestures`]).

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use aj_wire::SessionSummary;
use vaxis::cell::{Color, Style};
use vaxis::gwidth::{Method, gwidth};
use vaxis::mouse::{Button, Mouse, Type};
use vaxis::vxfw::{
    DrawContext, Event, EventContext, MaxSize, Overflow, RichText, Size, Surface, TextSpan, Widget,
};

use crate::text::one_line;
use crate::transcript::TranscriptStyles;

/// Columns the sidebar occupies when shown.
///
/// Fixed rather than proportional: a strip that grew with the terminal would
/// take width from the transcript for nothing. The columns go one to the focus
/// marker, one to the status glyph, one to a space, the rest to the label, and
/// the last two to a pad and the separator rule. Sized so the time of day
/// keeps its column and a hand-written tag still reads without eliding beside
/// it, which is most of what the strip is for once sessions carry names.
pub(crate) const SIDEBAR_COLS: u16 = 24;

/// Terminal width below which the strip holds itself back.
///
/// The strip is inflexible, so under this it would leave the transcript beside
/// it too little to read. Set so the transcript keeps at least a short line's
/// worth of columns.
pub(crate) const MIN_COLS_WITH_SIDEBAR: u16 = SIDEBAR_COLS + 20;

/// The focused row's marker, in the column left of the status glyph.
const FOCUS_MARKER: &str = "▌";

/// The rule between the strip and the transcript, drawn on every line of the
/// strip's height so the transcript's edge is one unbroken line.
const SEPARATOR: &str = "│";

/// What an unreachable host's header sets into its rule.
const UNREACHABLE_MARK: &str = " ! ─";

/// Columns a label may occupy: everything but the marker, the glyph, their
/// separating space, and the pad and rule on the right.
fn label_cols(width: u16) -> usize {
    usize::from(width.saturating_sub(5))
}

/// Columns the id-derived label takes where a tag shares the field: the width
/// of a minted id's `HH-MM-SS` time of day, which is what the column carries
/// on every session the app itself named.
const ID_COLS: usize = 8;

/// Columns between the id label and a tag beside it, so the two never run
/// together into one word.
const ID_TAG_GAP: usize = 1;

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

/// Where a row sits in the client's working set, which the strip encodes as
/// the label's brightness (spec 9.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Presence {
    /// The session on screen.
    Focused,
    /// Attached, folding frames while the user looks elsewhere.
    Background,
    /// Listed by the peer and nothing more.
    Listed,
}

/// One row of the sidebar.
pub(crate) struct SidebarRow {
    pub(crate) id: String,
    /// The name the user gave the session, shown in place of the id-derived
    /// label (spec 6.8).
    pub(crate) tag: Option<String>,
    /// Which enrolled host the row belongs to. `None` on a plain host's rows,
    /// which are all its own.
    pub(crate) host: Option<String>,
    pub(crate) status: RowStatus,
    /// Whether this is the session on screen.
    pub(crate) focused: bool,
    /// Whether the client folds frames for this session, which is what makes
    /// the working set legible (spec 9.2).
    pub(crate) attached: bool,
}

impl SidebarRow {
    /// This row's place in the working set.
    ///
    /// Brightness answers one question and no other: does the client hold
    /// this session open. Whether the peer can reach the host is the status
    /// glyph's axis, and letting it dim the row would put two facts into one
    /// encoding. An attached row on a host that has gone out therefore draws
    /// at attached brightness wearing the error glyph, which is exactly the
    /// truth of it: you have the session open and its host is not answering.
    /// Focus outranks attachment, because which session is on screen has to
    /// stay legible.
    fn presence(&self) -> Presence {
        if self.focused {
            Presence::Focused
        } else if self.attached {
            Presence::Background
        } else {
            Presence::Listed
        }
    }

    /// What the row shows in a label field of `cols` columns: the id-derived
    /// label in a fixed leading column, then the tag.
    ///
    /// The id column is what the strip is scanned down, so it holds one width
    /// and one place on every row and a tag supplements it rather than
    /// displacing it. A tag does not identify a session either (two can carry
    /// the same one), which is the other half of why it cannot have the
    /// column.
    ///
    /// A row with no tag has nothing to its right to hold the column against,
    /// so its label spreads over the whole field. That is what keeps a
    /// hand-named session, whose label is a filename rather than a time of
    /// day, readable.
    fn label(&self, cols: usize) -> String {
        let Some(tag) = &self.tag else {
            return field(&session_label(&self.id, cols), cols);
        };
        let id_cols = cols.min(ID_COLS);
        let id = session_label(&self.id, id_cols);
        if cols <= id_cols + ID_TAG_GAP {
            // Too narrow to hold a tag column at all. The tag is what drops:
            // half a time of day would leave every row's anchor unreadable,
            // and the strip is orientation before it is names.
            return field(&id, cols);
        }
        // A tag reads left to right, so an over-long one keeps its head and
        // says so with an ellipsis (`field` elides). An id is the other way
        // round: what distinguishes one is its tail (see [`session_label`]).
        field(&format!("{} {}", field(&id, id_cols), one_line(tag)), cols)
    }
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
    /// Where the wheel anchored the drawn run. `None` is the resting state, in
    /// which the run follows the focused row.
    scroll: Option<Anchor>,
}

/// Where the wheel left the drawn run, and the session that was focused when
/// it did.
///
/// The pairing is what makes a focus change drop the anchor. The layout
/// follows the focused row, and can only do that while nothing else holds the
/// run, so an anchor set before a switch would leave the user looking away
/// from the session they just opened. Carried alongside the anchor rather than
/// cleared on write so the rule holds however the mirror's rows arrive.
struct Anchor {
    /// Index into the display order the run starts at.
    at: usize,
    /// The session focused when the wheel set it.
    focused: Option<String>,
}

impl SidebarState {
    /// Whether the strip is drawn: wanted, and the terminal can spare it.
    pub(crate) fn shown(&self) -> bool {
        self.visible && !self.too_narrow
    }

    /// The wheel's anchor, if it still applies.
    fn anchor(&self) -> Option<usize> {
        self.scroll
            .as_ref()
            .filter(|anchor| anchor.focused.as_deref() == focused_id(&self.rows))
            .map(|anchor| anchor.at)
    }

    /// The strip's lines in a strip `height` lines tall.
    pub(crate) fn lines(&self, height: u16) -> Vec<StripLine> {
        strip_lines(&self.rows, height, self.anchor())
    }

    /// Move the drawn run `delta` rows in a strip `height` lines tall, and say
    /// whether it moved.
    ///
    /// The wheel anchors the run where it lands, because the layout otherwise
    /// follows the focused row and would pull the view straight back on the
    /// next frame. A wheel with nowhere to go (the rows all fit, or the run
    /// already sits against an end) leaves the anchor alone, so scrolling a
    /// strip that fits cannot quietly switch focus-following off.
    pub(crate) fn scroll_by(&mut self, delta: isize, height: u16) -> bool {
        // The create row is paid for before any run is chosen, exactly as in
        // [`strip_lines`].
        let budget = usize::from(height).saturating_sub(1);
        if budget == 0 {
            return false;
        }
        let layout = Layout::of(&self.rows);
        let from = layout.visible_run(budget, self.anchor()).start;
        let to = from
            .saturating_add_signed(delta)
            .min(layout.last_anchor(budget));
        if to == from {
            return false;
        }
        self.scroll = Some(Anchor {
            at: to,
            focused: focused_id(&self.rows).map(str::to_string),
        });
        true
    }
}

/// The session the rows say is on screen, if any.
fn focused_id(rows: &[SidebarRow]) -> Option<&str> {
    rows.iter()
        .find(|row| row.focused)
        .map(|row| row.id.as_str())
}

/// Build the display rows from the peer's directory.
///
/// Ordered by last activity, newest first, with ties broken by id descending so
/// the order is total: an unstable order would reshuffle under a user stepping
/// through it. `unseen` answers spec 6.8's "has it moved since I looked" for a
/// row the caller already holds, and `attached` answers "do I hold it open" for
/// a session id, which keeps this linear.
pub(crate) fn rows_for_display(
    rows: &[SessionSummary],
    focused: &str,
    unseen: impl Fn(&SessionSummary) -> bool,
    attached: impl Fn(&str) -> bool,
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
            attached: attached(&row.id),
            tag: row.tag.clone(),
            host: row.host.clone(),
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

/// One drawn line of the strip.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum StripLine {
    /// A host's group header, drawn above that host's rows.
    Header {
        host: String,
        /// Whether the peer can reach none of the host's rows.
        unreachable: bool,
    },
    /// A session row, named by its index into the rows the layout was built
    /// from, so a caller resolves it without re-deriving the display order.
    Session { index: usize },
    /// How many rows did not fit.
    Overflow { hidden: usize },
    /// The create affordance, always the last line.
    New,
}

/// What a pointer gesture on the strip asks for.
///
/// A gesture names the ask and nothing more, because it is a second trigger
/// for an action the chords already dispatch rather than a path of its own
/// (spec 9.2): the shell hands this to the same place the chord's handler
/// hands the session it stepped to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum StripGesture {
    /// Focus the session on the row that was clicked.
    Focus(String),
    /// Create a session.
    New,
}

/// Lay the strip out for `rows` in `height` lines.
///
/// The one place the height arithmetic lives. Host headers, the overflow row
/// and the create row all take lines away from the rows, and the focused row
/// has to survive that. Never returns more than `height` lines.
///
/// `scroll` is where the wheel anchored the run, or `None` to follow the
/// focused row (see [`SidebarState::scroll_by`]).
///
/// A height that cannot fit even the focused row and its header (two lines of
/// strip, with hosts to group) gives up on showing a row rather than
/// overrunning: the honest answer at that size is the overflow count.
pub(crate) fn strip_lines(
    rows: &[SidebarRow],
    height: u16,
    scroll: Option<usize>,
) -> Vec<StripLine> {
    let height = usize::from(height);
    if height == 0 {
        return Vec::new();
    }
    // The create row is always the last line, so it takes its line before
    // anything else competes for one.
    let budget = height - 1;
    if budget == 0 {
        return vec![StripLine::New];
    }
    let layout = Layout::of(rows);
    layout.lines(layout.visible_run(budget, scroll))
}

/// A run of rows sharing a host.
struct Group<'a> {
    /// `None` on rows from a plain host, which are all its own.
    host: Option<&'a str>,
    /// Whether the peer can reach none of them, which the header says once
    /// instead of the user reading it off every row.
    unreachable: bool,
    /// Where the group's rows sit in [`Layout::order`].
    span: Range<usize>,
}

/// The display order of the rows and the host groups over it.
struct Layout<'a> {
    rows: &'a [SidebarRow],
    /// Row indices in display order: activity order, gathered into groups.
    order: Vec<usize>,
    /// The groups, each a contiguous span of [`Self::order`].
    groups: Vec<Group<'a>>,
    /// Whether groups wear headers at all. One host, or none, is not a
    /// grouping: a plain single-host connect has to look exactly as it would
    /// have before hosts existed.
    headed: bool,
}

impl<'a> Layout<'a> {
    fn of(rows: &'a [SidebarRow]) -> Self {
        // Rows arrive activity-ordered, so gathering by first appearance
        // orders the hosts by their most recent activity and leaves each
        // group's own rows in activity order.
        let mut hosts: Vec<Option<&str>> = Vec::new();
        for row in rows {
            let host = row.host.as_deref();
            if !hosts.contains(&host) {
                hosts.push(host);
            }
        }
        // A group with no host name gets no header, and a headerless run under
        // someone else's header would read as theirs, so it sorts first. Only
        // reachable in a mixed directory, which a gateway does not produce.
        hosts.sort_by_key(Option::is_some);
        let mut order = Vec::with_capacity(rows.len());
        let mut groups = Vec::with_capacity(hosts.len());
        for host in hosts {
            let start = order.len();
            let mut unreachable = true;
            for (index, row) in rows.iter().enumerate() {
                if row.host.as_deref() != host {
                    continue;
                }
                unreachable &= row.status == RowStatus::Unreachable;
                order.push(index);
            }
            groups.push(Group {
                host,
                unreachable,
                span: start..order.len(),
            });
        }
        let headed = groups.len() > 1;
        Self {
            rows,
            order,
            groups,
            headed,
        }
    }

    /// The header a run reaching into `group` draws for it, if any.
    fn header_of(&self, group: &Group<'a>, run: &Range<usize>) -> Option<&'a str> {
        if !self.headed {
            return None;
        }
        let host = group.host?;
        (group.span.start < run.end && run.start < group.span.end).then_some(host)
    }

    /// Lines the run `order[run]` occupies: one per row, one per header it
    /// reaches, and one for the overflow row when it leaves any row out. The
    /// create row is not counted, it is paid for before a run is chosen.
    fn cost(&self, run: Range<usize>) -> usize {
        let headers = self
            .groups
            .iter()
            .filter(|group| self.header_of(group, &run).is_some())
            .count();
        headers + run.len() + usize::from(run.len() < self.order.len())
    }

    /// The run of the display order to draw in `budget` lines: the longest one
    /// that fits, anchored where `scroll` says or around the focused row when
    /// it says nothing.
    ///
    /// Following focus scrolls by the least it can. The run stays anchored at
    /// the top until focus would fall past its bottom edge, which keeps a step
    /// from jumping the whole strip. With no focused row it shows the top,
    /// which is the most recently active end.
    fn visible_run(&self, budget: usize, scroll: Option<usize>) -> Range<usize> {
        // An anchor the user set outranks the focused row: they scrolled to
        // look elsewhere, and following focus would undo that on the very next
        // frame. The anchor lapses on a focus change (see [`Anchor`]), which is
        // what hands the strip back to focus-following.
        if let Some(start) = scroll {
            return self.run_from(budget, start.min(self.last_anchor(budget)));
        }
        let top = self.run_from(budget, 0);
        let Some(focus) = self
            .order
            .iter()
            .position(|&index| self.rows[index].focused)
        else {
            return top;
        };
        if focus < top.end {
            return top;
        }
        // Focus fell past the bottom edge, so the run ends on it and reaches
        // back as far as the budget allows. An empty run is the answer when
        // even one row and its header will not fit (see [`strip_lines`]).
        (0..=focus)
            .find(|&start| self.cost(start..focus + 1) <= budget)
            .map_or(focus..focus, |start| start..focus + 1)
    }

    /// The longest run beginning at `start` that fits `budget`.
    ///
    /// Scanned downward because cost is not monotone in the run's length: the
    /// run holding every row spends no line on the overflow row, so it can fit
    /// where a run one row shorter does not.
    fn run_from(&self, budget: usize, start: usize) -> Range<usize> {
        (start..=self.order.len())
            .rev()
            .find(|&end| self.cost(start..end) <= budget)
            .map_or(start..start, |end| start..end)
    }

    /// The furthest down the run can be anchored: the first start whose run
    /// still reaches the last row. Anchoring past it would redraw the same
    /// last page, so this is where the wheel stops.
    fn last_anchor(&self, budget: usize) -> usize {
        let total = self.order.len();
        // An empty run costs only the overflow line, so with a budget of at
        // least one line the scan always terminates.
        (0..=total)
            .find(|&start| self.cost(start..total) <= budget)
            .unwrap_or(total)
    }

    /// The lines for a run, in draw order.
    fn lines(&self, run: Range<usize>) -> Vec<StripLine> {
        let mut lines = Vec::with_capacity(run.len() + self.groups.len() + 2);
        for group in &self.groups {
            let from = group.span.start.max(run.start);
            let to = group.span.end.min(run.end);
            if from >= to {
                continue;
            }
            if let Some(host) = self.header_of(group, &run) {
                lines.push(StripLine::Header {
                    host: host.to_string(),
                    unreachable: group.unreachable,
                });
            }
            lines.extend((from..to).map(|at| StripLine::Session {
                index: self.order[at],
            }));
        }
        let hidden = self.order.len() - run.len();
        if hidden > 0 {
            lines.push(StripLine::Overflow { hidden });
        }
        lines.push(StripLine::New);
        lines
    }
}

/// The visible part of a session id: its time of day.
///
/// Minted ids are `YYYY-MM-DD-HH-MM-SS-mmm`, and the date is the same for
/// every session the user is likely to be juggling, so the leading date earns
/// none of the width it costs. An id in any other shape (a hand-renamed file)
/// is shown from its tail, where what distinguishes such a name usually is.
///
/// `cols` is a budget in display columns, not characters, so a label of wide
/// graphemes cannot overflow its column and wrap. Wrapping would break the
/// strip's one-line-per-row correspondence and misattribute every row below
/// it. Control characters are dropped for the same reason: a session file may
/// be named anything a filesystem accepts.
///
/// NOTE: two sessions minted in the same second share a label. The strip is
/// for orientation, not identification, and the header carries the focused
/// session's id in full.
pub(crate) fn session_label(id: &str, cols: usize) -> String {
    let label = minted_time(id).map_or_else(|| one_line(id), |time| time.join("-"));
    truncate_to_cols(&label, cols)
}

/// The hour, minute and second a minted `YYYY-MM-DD-HH-MM-SS-mmm` id carries.
///
/// `None` for an id in any other shape, which has no time in it to read. The
/// shape is checked component by component rather than by counting dashes: a
/// hand-named session can have as many, and slicing its middle out would show
/// a fragment of a word.
fn minted_time(id: &str) -> Option<[String; 3]> {
    let cleaned = one_line(id);
    let parts: Vec<&str> = cleaned.split('-').collect();
    let minted = parts.len() == 7
        && parts[0].len() == 4
        && parts[..6]
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()));
    minted.then(|| std::array::from_fn(|at| parts[3 + at].to_string()))
}

/// Display columns `text` occupies.
fn width_of(text: &str) -> usize {
    usize::from(gwidth(text, Method::Unicode))
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

/// Trim `text` to `cols` display columns from its tail, marking the cut with an
/// ellipsis, which keeps the head where a written name says what it is.
fn elide_to_cols(text: &str, cols: usize) -> String {
    let Ok(budget) = u16::try_from(cols) else {
        return text.to_string();
    };
    if gwidth(text, Method::Unicode) <= budget {
        return text.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut out = String::new();
    for ch in text.chars() {
        let mut candidate = String::with_capacity(out.len() + 4);
        candidate.push_str(&out);
        candidate.push(ch);
        // The ellipsis has to fit beside whatever is kept.
        if gwidth(&candidate, Method::Unicode) + 1 > budget {
            break;
        }
        out = candidate;
    }
    out.push('…');
    out
}

/// `text` in a field of exactly `cols` display columns, elided if it is too
/// wide and padded with spaces if it is too narrow.
///
/// Every line is built from these, which is what keeps the separator in its
/// column whatever a tag or a host is named.
fn field(text: &str, cols: usize) -> String {
    let mut out = elide_to_cols(text, cols);
    out.push_str(&" ".repeat(cols.saturating_sub(width_of(&out))));
    out
}

/// A host header's label field: the name, then a rule out to the field's edge,
/// with the unreachable mark set into the rule's tail.
///
/// The mark rides inside the rule rather than hanging off its end because the
/// rule has to reach the strip's edge either way.
fn header_field(host: &str, unreachable: bool, cols: usize) -> String {
    let mark = if unreachable { UNREACHABLE_MARK } else { "" };
    let mark_cols = mark.chars().count();
    // The name, one space, one rule character, and the mark, in that order.
    let name = elide_to_cols(&one_line(host), cols.saturating_sub(mark_cols + 2));
    let rule = cols.saturating_sub(width_of(&name) + 1 + mark_cols);
    format!("{name} {}{mark}", "─".repeat(rule))
}

/// The sidebar strip.
pub(crate) struct SessionSidebar {
    state: Rc<RefCell<SidebarState>>,
    styles: Rc<TranscriptStyles>,
    /// The band painted behind the line under the pointer.
    hover_bg: Color,
    /// The strip's height at the last draw, which is what the wheel scrolls
    /// against.
    height: u16,
    /// What a click on each line the last draw painted asks for, `None` for a
    /// line that offers nothing.
    ///
    /// Recorded by the draw that painted it, rather than resolved when the
    /// press arrives: the drive loop refreshes the mirror at the top of every
    /// iteration and paints only once the frame budget has elapsed, so a
    /// press handled in between would resolve a line against rows that are
    /// not the ones on screen.
    gestures: Vec<Option<StripGesture>>,
    /// The line under the pointer, `None` when the pointer is elsewhere.
    hover: Option<u16>,
    /// Where a resolved gesture goes. Unset until the shell wires it, which
    /// leaves the strip inert rather than half-wired.
    on_gesture: Option<Box<dyn FnMut(&mut EventContext, StripGesture)>>,
}

impl SessionSidebar {
    pub(crate) fn new(
        state: Rc<RefCell<SidebarState>>,
        styles: Rc<TranscriptStyles>,
        hover_bg: Color,
    ) -> Self {
        Self {
            state,
            styles,
            hover_bg,
            height: 0,
            gestures: Vec::new(),
            hover: None,
            on_gesture: None,
        }
    }

    /// Wire where a resolved pointer gesture goes.
    ///
    /// The gesture reaches the same sink the stepping and create chords
    /// reach, rather than being dispatched as the action they dispatch, so it
    /// does not pass the keymap predicate that makes those chords inert under
    /// an overlay. Two things make that sound rather than accidental.
    ///
    /// A click cannot reach the strip while an overlay is up. The shell draws
    /// a full-viewport scrim above the base layout for as long as the stack is
    /// non-empty, and the scrim is then the deepest hit everywhere and
    /// consumes the press at target, which is before the strip's turn in the
    /// bubbling phase. The strip sees the press in the capturing phase only,
    /// where it deliberately does nothing but drop its band.
    ///
    /// And a gesture names a session, which no action carries: a chord steps
    /// the order, a click points at a row. The two can only meet at the
    /// request they park, which is where the switch actually happens.
    pub(crate) fn set_on_gesture(
        &mut self,
        on_gesture: Box<dyn FnMut(&mut EventContext, StripGesture)>,
    ) {
        self.on_gesture = Some(on_gesture);
    }

    /// What a click on the strip's `line`th row asks for.
    ///
    /// `None` on the lines that are not affordances: a host header and the
    /// overflow count, which say something rather than offer something, and
    /// every line below the strip's content, which the layout leaves out of
    /// its result so those resolve to nothing by construction.
    ///
    /// A row that is already focused resolves like any other. Resuming the
    /// session on screen is a no-op where the switch happens, and stating that
    /// rule twice is how the two copies drift apart.
    fn gesture_at(&self, line: u16) -> Option<StripGesture> {
        self.gestures.get(usize::from(line))?.clone()
    }

    /// Point the hover band at `line`, and say whether the band moved.
    ///
    /// The band only marks a line a click acts on: over a header it would
    /// offer something that is not there.
    fn set_hover(&mut self, line: Option<u16>) -> bool {
        let line = line.filter(|&line| {
            self.gestures
                .get(usize::from(line))
                .is_some_and(Option::is_some)
        });
        let moved = self.hover != line;
        self.hover = line;
        moved
    }

    /// A pointer event at the strip's own coordinates.
    ///
    /// Every report moves the band, and only a left press and the wheel do
    /// anything more. A redraw is asked for exactly when something changed,
    /// which is what keeps a pointer resting on the strip from arming a frame
    /// per report.
    fn on_mouse(&mut self, ctx: &mut EventContext, mouse: Mouse) {
        // A negative row is off the top of the strip, which is nowhere.
        let line = u16::try_from(mouse.row).ok();
        if self.set_hover(line) {
            ctx.redraw = true;
        }
        match mouse.button {
            Button::WheelUp => self.wheel(ctx, -1),
            Button::WheelDown => self.wheel(ctx, 1),
            Button::Left if mouse.kind == Type::Press => {
                let Some(gesture) = line.and_then(|line| self.gesture_at(line)) else {
                    return;
                };
                if let Some(on_gesture) = self.on_gesture.as_mut() {
                    on_gesture(ctx, gesture);
                }
                // The strip acted on the press, so nothing else should.
                ctx.consume_event();
            }
            _ => {}
        }
    }

    /// Wheel the drawn run by `delta` rows, if it has anywhere to go.
    fn wheel(&self, ctx: &mut EventContext, delta: isize) {
        if self.state.borrow_mut().scroll_by(delta, self.height) {
            ctx.consume_and_redraw();
        }
    }

    /// The style a label is drawn in: the working-set axis, as brightness.
    fn label_style(&self, row: &SidebarRow) -> Style {
        match row.presence() {
            Presence::Focused => self.styles.accent,
            Presence::Background => self.styles.text,
            Presence::Listed => self.styles.dim,
        }
    }

    /// The style a status glyph is drawn in.
    ///
    /// It does not vary with attachment: what a session is doing is the same
    /// fact whether or not the client holds it open.
    fn glyph_style(&self, status: RowStatus) -> Style {
        match status {
            RowStatus::Unreachable => self.styles.error,
            RowStatus::Working => self.styles.success,
            RowStatus::Unseen => self.styles.warning,
            RowStatus::Idle => self.styles.dim,
        }
    }

    /// One line's spans: the focus marker, the status glyph, the label field,
    /// then the pad and the separator. Every line has this shape, so a column
    /// means the same thing on all of them.
    fn line_spans(
        &self,
        line: &StripLine,
        rows: &[SidebarRow],
        width: u16,
        hovered: bool,
    ) -> Vec<TextSpan> {
        let cols = label_cols(width);
        let dim = self.styles.dim;
        let (marker, glyph, glyph_style, label, label_style) = match line {
            StripLine::Header { host, unreachable } => (
                " ",
                "~",
                dim,
                field(&header_field(host, *unreachable, cols), cols),
                dim,
            ),
            StripLine::Session { index } => {
                let row = &rows[*index];
                (
                    if row.focused { FOCUS_MARKER } else { " " },
                    row.status.glyph(),
                    self.glyph_style(row.status),
                    row.label(cols),
                    self.label_style(row),
                )
            }
            StripLine::Overflow { hidden } => {
                (" ", " ", dim, field(&format!("…{hidden} more"), cols), dim)
            }
            StripLine::New => (" ", "+", dim, field("new", cols), dim),
        };
        // The band reaches the pad and stops short of the separator, which
        // belongs to the rule down the strip's edge rather than to any row.
        let tint = |style: Style| {
            if hovered {
                Style {
                    bg: self.hover_bg,
                    ..style
                }
            } else {
                style
            }
        };
        // The whole label field carries one brightness, the tag included:
        // brightness answers where the row sits in the working set, and a
        // part of the field that did not move with it would look like a
        // second answer to the same question.
        vec![
            span(marker, tint(self.styles.accent)),
            span(glyph, tint(glyph_style)),
            span(&format!(" {label}"), tint(label_style)),
            span(" ", tint(dim)),
            span(SEPARATOR, dim),
        ]
    }

    /// A line below the last drawn one: nothing but the separator, which runs
    /// the strip's full height so the transcript's edge is one unbroken rule.
    fn blank_spans(&self, width: u16) -> Vec<TextSpan> {
        let pad = " ".repeat(usize::from(width) - 1);
        vec![span(&format!("{pad}{SEPARATOR}"), self.styles.dim)]
    }
}

/// What a click on `line` asks for, resolved against the rows the layout that
/// produced it was built from.
fn gesture_for(line: &StripLine, rows: &[SidebarRow]) -> Option<StripGesture> {
    match line {
        StripLine::Session { index } => Some(StripGesture::Focus(rows.get(*index)?.id.clone())),
        StripLine::New => Some(StripGesture::New),
        StripLine::Header { .. } | StripLine::Overflow { .. } => None,
    }
}

fn span(text: &str, style: Style) -> TextSpan {
    TextSpan {
        text: text.to_string(),
        style,
        ..TextSpan::default()
    }
}

impl Widget for SessionSidebar {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // A flex row measures its inflexible children under an unbounded width,
        // so the width has to come from this widget rather than the context:
        // that measurement is exactly the question "how wide are you".
        let width = ctx
            .max
            .width
            .map_or(SIDEBAR_COLS, |max| max.min(SIDEBAR_COLS));
        if !self.state.borrow().shown() || width == 0 {
            // Nothing is drawn, so nothing is there to gesture at either.
            self.gestures.clear();
            return Surface::with_size(Size {
                width: 0,
                height: 0,
            });
        }
        // An unbounded height is a measurement pass, which reads back only the
        // width: lay every line out and let the surface be as tall as it is.
        self.height = ctx.max.height.unwrap_or(u16::MAX);
        let lines = {
            let state = self.state.borrow();
            let lines = state.lines(self.height);
            // Resolved here, against the rows this paint draws from, so a
            // press that arrives before the next paint answers with what is
            // on screen (see [`Self::gestures`]).
            self.gestures = lines
                .iter()
                .map(|line| gesture_for(line, &state.rows))
                .collect();
            lines
        };
        // The rows can move under a pointer that has not, so the band is
        // re-resolved against the fresh layout rather than left marking
        // whatever used to be on that line.
        self.set_hover(self.hover);
        let state = self.state.borrow();
        let hover = self.hover.map(usize::from);
        let blanks = usize::from(ctx.max.height.unwrap_or(0)).saturating_sub(lines.len());
        let mut spans: Vec<TextSpan> = Vec::with_capacity((lines.len() + blanks) * 6);
        for index in 0..lines.len() + blanks {
            if index > 0 {
                spans.push(span("\n", self.styles.text));
            }
            match lines.get(index) {
                Some(line) => {
                    spans.extend(self.line_spans(line, &state.rows, width, hover == Some(index)));
                }
                None => spans.extend(self.blank_spans(width)),
            }
        }
        // No soft wrap: one drawn row per line is the strip's contract, and a
        // wrapped line would misattribute every line below it.
        let mut text = RichText {
            softwrap: false,
            overflow: Overflow::Clip,
            ..RichText::new(spans)
        };
        let mut surface = text.draw(&ctx.with_constraints(
            Size { width, height: 0 },
            MaxSize {
                width: Some(width),
                height: ctx.max.height,
            },
        ));
        // The strip owns its full column width even where a line is shorter, so
        // the transcript beside it never starts mid-strip.
        surface.size.width = width;
        surface
    }

    fn capture_event(&mut self, ctx: &mut EventContext, event: &Event) {
        // The strip only reaches the capturing phase when something floats
        // above it (an open overlay's scrim, say). That gesture belongs to
        // whatever is on top, so the strip stays out of it and drops its band
        // rather than pointing at a row the click will not reach.
        if matches!(event, Event::Mouse(_)) && self.set_hover(None) {
            ctx.redraw = true;
        }
    }

    fn handle_event(&mut self, ctx: &mut EventContext, event: &Event) {
        match event {
            Event::Mouse(mouse) => self.on_mouse(ctx, *mouse),
            // The pointer left the strip, so the band goes with it.
            Event::MouseLeave => {
                if self.set_hover(None) {
                    ctx.redraw = true;
                }
            }
            _ => {}
        }
    }

    fn wants_events(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_id_shows_its_time_of_day() {
        assert_eq!(
            session_label("2026-08-06-19-07-19-368", 12),
            "19-07-19",
            "the date is the same for every session in a sitting, so it is dropped",
        );
    }

    /// An id in another shape (a hand-renamed session file) is shown from its
    /// tail, which is where what distinguishes such a name usually is.
    #[test]
    fn an_unfamiliar_id_is_shown_from_its_tail() {
        assert_eq!(session_label("short", 12), "short");
        assert_eq!(
            session_label("a-very-long-hand-named-session", 8),
            "-session"
        );
    }

    /// Seven dashes do not make a minted id. A hand-named session with as many
    /// components must not have its middle sliced out and shown as a label.
    #[test]
    fn a_hand_named_id_with_seven_parts_is_still_shown_from_its_tail() {
        assert_eq!(
            session_label("notes-on-the-rust-borrow-checker-draft", 12),
            "hecker-draft",
        );
    }

    /// A collision suffix rides the id's last component, so the id still reads
    /// as the minted shape and still shows its time of day.
    #[test]
    fn a_collision_suffix_still_reads_as_a_minted_id() {
        assert_eq!(session_label("2026-08-06-19-07-19-368_2", 12), "19-07-19");
    }

    /// A label is budgeted in display columns, so wide graphemes cannot overflow
    /// the strip and wrap onto the next row's line.
    #[test]
    fn a_wide_character_label_fits_its_columns() {
        let label = session_label("会話ノート記録帳", 12);
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
    /// below it. A tag comes off the wire, so it gets the same treatment.
    #[test]
    fn a_control_character_cannot_split_a_row() {
        let label = session_label("first\nsecond", 12);
        assert!(!label.contains('\n'), "{label:?} would break the row");
        let tagged = row("session-1").tag("first\nsecond").build().label(12);
        assert!(!tagged.contains('\n'), "{tagged:?} would break the row");
        assert!(
            !header_field("first\nsecond", false, 19).contains('\n'),
            "and so would a host name",
        );
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
            tag: None,
            host: None,
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
        let display = rows_for_display(&rows, "session-b", |_| false, |_| false);
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
        let display = rows_for_display(&[a, b], "session-a", |_| false, |_| false);
        assert_eq!(ids(&display), vec!["session-b", "session-a"]);
    }

    /// The directory's answers ride into the row: what the peer says about the
    /// session, and what this client holds open (spec 9.2).
    #[test]
    fn a_row_carries_the_tag_the_host_and_the_attachment() {
        let mut tagged = at("session-a", 0);
        tagged.tag = Some("fix-auth".to_string());
        tagged.host = Some("builder-1".to_string());
        let display = rows_for_display(
            &[tagged, at("session-b", 1)],
            "session-b",
            |_| false,
            |id| id == "session-a",
        );
        assert_eq!(display[0].tag.as_deref(), Some("fix-auth"));
        assert_eq!(display[0].host.as_deref(), Some("builder-1"));
        assert_eq!(display[0].presence(), Presence::Background);
        assert_eq!(
            display[1].presence(),
            Presence::Focused,
            "the focused row outranks its attachment",
        );
    }

    /// A row builder: everything the layout reads, defaulted to the plain case.
    struct Build {
        row: SidebarRow,
    }

    fn row(id: &str) -> Build {
        Build {
            row: SidebarRow {
                id: id.to_string(),
                tag: None,
                host: None,
                status: RowStatus::Idle,
                focused: false,
                attached: false,
            },
        }
    }

    impl Build {
        fn tag(mut self, tag: &str) -> Self {
            self.row.tag = Some(tag.to_string());
            self
        }

        fn host(mut self, host: &str) -> Self {
            self.row.host = Some(host.to_string());
            self
        }

        fn status(mut self, status: RowStatus) -> Self {
            self.row.status = status;
            self
        }

        fn focused(mut self) -> Self {
            self.row.focused = true;
            self
        }

        fn attached(mut self) -> Self {
            self.row.attached = true;
            self
        }

        fn build(self) -> SidebarRow {
            self.row
        }
    }

    fn rows_named(ids: &[&str]) -> Vec<SidebarRow> {
        ids.iter().map(|id| row(id).build()).collect()
    }

    fn state_of(focused_at: usize, len: usize) -> SidebarState {
        SidebarState {
            visible: true,
            rows: (0..len)
                .map(|i| {
                    let built = row(&format!("session-{i}"));
                    if i == focused_at {
                        built.focused()
                    } else {
                        built
                    }
                    .build()
                })
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
            rows: rows_named(&["session-0", "session-1"]),
            ..SidebarState::default()
        };
        assert_eq!(step_session(&unfocused, true), None);
    }

    /// The rows a layout draws, by id, in the order the lines come out.
    fn drawn<'a>(lines: &[StripLine], rows: &'a [SidebarRow]) -> Vec<&'a str> {
        lines
            .iter()
            .filter_map(|line| match line {
                StripLine::Session { index } => Some(rows[*index].id.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The headers a layout draws, in order.
    fn headers(lines: &[StripLine]) -> Vec<(&str, bool)> {
        lines
            .iter()
            .filter_map(|line| match line {
                StripLine::Header { host, unreachable } => Some((host.as_str(), *unreachable)),
                _ => None,
            })
            .collect()
    }

    fn hidden(lines: &[StripLine]) -> Option<usize> {
        lines.iter().find_map(|line| match line {
            StripLine::Overflow { hidden } => Some(*hidden),
            _ => None,
        })
    }

    /// The create row is the last line, always, whatever else the strip had to
    /// leave out. It is the affordance a pointer aims at, so it cannot move.
    #[test]
    fn the_create_row_is_always_the_last_line() {
        for (rows, height) in [
            (rows_named(&[]), 1),
            (rows_named(&["session-0"]), 20),
            (rows_named(&["a", "b", "c", "d"]), 3),
            (rows_named(&["a", "b", "c", "d"]), 1),
        ] {
            let lines = strip_lines(&rows, height, None);
            assert_eq!(
                lines.last(),
                Some(&StripLine::New),
                "{} rows in {height} lines: {lines:?}",
                rows.len(),
            );
            assert!(
                lines.len() <= usize::from(height),
                "and the layout stayed inside its height: {lines:?}",
            );
        }
    }

    /// A height of nothing draws nothing, rather than panicking on the arithmetic
    /// that pays for the create row.
    #[test]
    fn a_zero_height_strip_draws_nothing() {
        assert!(strip_lines(&rows_named(&["a", "b"]), 0, None).is_empty());
    }

    /// One host, or none at all, is not a grouping. A plain connect has to look
    /// exactly as it would have before hosts existed (spec 9.2).
    #[test]
    fn a_single_host_gets_no_headers() {
        let hostless = rows_named(&["a", "b", "c"]);
        assert!(
            headers(&strip_lines(&hostless, 20, None)).is_empty(),
            "rows with no host name nothing to group under",
        );
        let one_host: Vec<SidebarRow> = ["a", "b", "c"]
            .iter()
            .map(|id| row(id).host("builder-1").build())
            .collect();
        let lines = strip_lines(&one_host, 20, None);
        assert!(
            headers(&lines).is_empty(),
            "one host is not a grouping: {lines:?}",
        );
        assert_eq!(drawn(&lines, &one_host), vec!["a", "b", "c"]);
    }

    /// Distinct hosts group, hosts in order of their most recent activity and
    /// rows in activity order inside each group.
    #[test]
    fn distinct_hosts_group_by_most_recent_activity() {
        // Activity order interleaves the hosts, so a layout that merely kept
        // the input order, or that sorted the hosts by name, would differ.
        let rows = vec![
            row("laptop-new").host("laptop").build(),
            row("builder-new").host("builder-1").build(),
            row("laptop-old").host("laptop").build(),
            row("builder-old").host("builder-1").build(),
        ];
        let lines = strip_lines(&rows, 20, None);
        assert_eq!(
            headers(&lines),
            vec![("laptop", false), ("builder-1", false)]
        );
        assert_eq!(
            drawn(&lines, &rows),
            vec!["laptop-new", "laptop-old", "builder-new", "builder-old"],
        );
        // And the header sits above its own rows, not somewhere in the list.
        assert!(matches!(lines[0], StripLine::Header { .. }));
        assert!(matches!(lines[3], StripLine::Header { .. }));
    }

    /// A host the peer cannot reach says so once on its header. One reachable
    /// row is enough to take the mark off: the host is answering.
    #[test]
    fn an_unreachable_host_marks_its_header() {
        let mut rows = vec![
            row("gone-1")
                .host("laptop")
                .status(RowStatus::Unreachable)
                .build(),
            row("gone-2")
                .host("laptop")
                .status(RowStatus::Unreachable)
                .build(),
            row("here").host("builder-1").build(),
        ];
        assert_eq!(
            headers(&strip_lines(&rows, 20, None)),
            vec![("laptop", true), ("builder-1", false)],
        );
        rows[1].status = RowStatus::Idle;
        assert_eq!(
            headers(&strip_lines(&rows, 20, None)),
            vec![("laptop", false), ("builder-1", false)],
            "a host with one reachable session is reachable",
        );
    }

    /// A directory that names a host on some rows and not others still reads:
    /// the nameless rows go first, so a headerless run never sits under
    /// someone else's header and looks like theirs.
    #[test]
    fn rows_without_a_host_sort_above_the_headers() {
        let rows = vec![
            row("named").host("builder-1").build(),
            row("nameless").build(),
        ];
        let lines = strip_lines(&rows, 20, None);
        assert_eq!(drawn(&lines, &rows), vec!["nameless", "named"]);
        assert_eq!(headers(&lines), vec![("builder-1", false)]);
        assert!(
            matches!(lines[0], StripLine::Session { .. }),
            "the nameless row is above every header: {lines:?}",
        );
    }

    /// Rows that do not fit are counted, not dropped in silence. What is cut is
    /// the least recently active end, because that is how the rows are ordered.
    #[test]
    fn the_rows_that_do_not_fit_are_counted() {
        let rows = rows_named(&["a", "b", "c", "d", "e"]);
        // Six lines: five rows and the create row, so nothing is cut.
        let whole = strip_lines(&rows, 6, None);
        assert_eq!(drawn(&whole, &rows), vec!["a", "b", "c", "d", "e"]);
        assert_eq!(hidden(&whole), None, "nothing was left out: {whole:?}");

        // One line fewer, and the overflow row costs one of its own: four
        // lines of budget hold three rows and the count of the other two.
        let cut = strip_lines(&rows, 5, None);
        assert_eq!(drawn(&cut, &rows), vec!["a", "b", "c"]);
        assert_eq!(hidden(&cut), Some(2));
        assert_eq!(cut.len(), 5, "and it used every line it had: {cut:?}");
    }

    /// A host header takes a line from the rows like anything else, and the
    /// count of what is left out has to be right across that.
    #[test]
    fn headers_come_out_of_the_same_budget() {
        let rows = vec![
            row("a").host("one").build(),
            row("b").host("one").build(),
            row("c").host("two").build(),
            row("d").host("two").build(),
        ];
        // Five lines: create row, one header, two rows, one overflow row.
        let lines = strip_lines(&rows, 5, None);
        assert_eq!(drawn(&lines, &rows), vec!["a", "b"]);
        assert_eq!(headers(&lines), vec![("one", false)]);
        assert_eq!(hidden(&lines), Some(2));

        // Seven lines fit both headers, all four rows and the create row.
        let whole = strip_lines(&rows, 7, None);
        assert_eq!(drawn(&whole, &rows), vec!["a", "b", "c", "d"]);
        assert_eq!(headers(&whole), vec![("one", false), ("two", false)]);
        assert_eq!(hidden(&whole), None);
    }

    /// The focused row is drawn even when the store holds more sessions than
    /// the terminal has lines, and the count still names every row left out,
    /// above the run as well as below it.
    #[test]
    fn the_focused_row_stays_visible() {
        let mut rows = rows_named(&["a", "b", "c", "d", "e", "f"]);
        rows[4].focused = true;
        // Four lines: the create row, the overflow row, and two rows ending on
        // the focused one.
        let lines = strip_lines(&rows, 4, None);
        assert_eq!(drawn(&lines, &rows), vec!["d", "e"]);
        assert_eq!(hidden(&lines), Some(4), "four rows are out of view");
    }

    /// It scrolls by the least it can: while focus still fits above the bottom
    /// edge the run stays at the top, so a step does not jump the strip. The
    /// run reaches past the focused row to fill the height, it does not stop
    /// on it.
    #[test]
    fn the_run_holds_still_while_focus_fits() {
        let mut rows = rows_named(&["a", "b", "c", "d", "e", "f"]);
        rows[0].focused = true;
        assert_eq!(
            drawn(&strip_lines(&rows, 4, None), &rows),
            vec!["a", "b"],
            "the row below the focused one fills the line it left",
        );
        rows[0].focused = false;
        rows[1].focused = true;
        assert_eq!(drawn(&strip_lines(&rows, 4, None), &rows), vec!["a", "b"]);
    }

    /// The minted id every layout test reads its time of day out of.
    const MINTED: &str = "2026-08-06-19-07-19-368";

    /// The time column holds one width and one place on every row: a tag
    /// supplements it to the right and never displaces it. That is what makes
    /// the column scannable, and it is why the tag is the part that elides.
    #[test]
    fn the_time_column_is_the_same_on_a_tagged_and_an_untagged_row() {
        let untagged = row(MINTED).build().label(19);
        assert_eq!(untagged, field("19-07-19", 19));
        for tag in ["", "fix-auth", "rewrite-the-gateway-provisioner", "会話"] {
            let tagged = row(MINTED).tag(tag).build().label(19);
            assert!(
                tagged.starts_with("19-07-19 "),
                "{tag:?} moved the time column: {tagged:?}",
            );
            assert_eq!(width_of(&tagged), 19, "{tag:?} overran the field");
        }
    }

    /// An untagged row lays out exactly as it did before tags existed: with
    /// nothing to its right holding the column, the id label has the whole
    /// field, which is what keeps a hand-named session readable.
    #[test]
    fn an_untagged_row_has_the_whole_field() {
        for id in [MINTED, "notes-on-the-rust-borrow-checker-draft"] {
            assert_eq!(
                row(id).build().label(19),
                field(&session_label(id, 19), 19),
                "{id:?} did not lay out as a plain label",
            );
        }
    }

    /// A row shows the name the user gave it in the column beside the time,
    /// and an over-long one keeps its head and says it was cut.
    #[test]
    fn a_tag_takes_the_column_beside_the_time() {
        assert_eq!(
            row(MINTED).tag("fix-auth").build().label(19),
            "19-07-19 fix-auth  "
        );
        // Ten columns is what the field leaves a tag, and a tag that fills
        // them exactly is not cut.
        assert_eq!(
            row(MINTED).tag("ten-column").build().label(19),
            "19-07-19 ten-column"
        );
        let long = row(MINTED).tag("rewrite-the-gateway-provisioner").build();
        assert_eq!(long.label(19), "19-07-19 rewrite-t\u{2026}");
        assert_eq!(
            width_of(&long.label(19)),
            19,
            "the ellipsis is inside the tag's column, not beside it",
        );
    }

    /// A hand-renamed session has no time of day to show, so its id label
    /// takes the column the time would have had. The column means "which
    /// session" on every row, and a tag never moves it.
    #[test]
    fn a_hand_named_session_keeps_the_id_column() {
        let named = row("notes-on-the-rust-borrow-checker-draft")
            .tag("fix-auth")
            .build();
        assert_eq!(named.label(19), "er-draft fix-auth  ");
    }

    /// A tag of wide graphemes is budgeted in display columns, so it cannot
    /// overflow the field and push the separator off its column. Combining
    /// marks take no columns of their own and emoji take two, and both ride
    /// into a tag off the wire.
    #[test]
    fn a_wide_tag_fits_its_columns() {
        for tag in [
            "会話ノート記録帳の下書き",
            "e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}e\u{301}",
            "🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀🚀",
            "🚀-会話-e\u{301}-🚀-会話",
        ] {
            for cols in [9, 12, 19] {
                let drawn = row(MINTED).tag(tag).build().label(cols);
                assert_eq!(
                    width_of(&drawn),
                    cols,
                    "{tag:?} in {cols} columns drew {drawn:?}",
                );
                assert!(
                    drawn.starts_with("19-07-19"),
                    "and the time kept its column: {drawn:?}",
                );
            }
        }
        let cut = row(MINTED).tag("会話ノート記録").build().label(19);
        assert!(
            cut.trim_end().ends_with('\u{2026}'),
            "and it says it was cut: {cut:?}",
        );
    }

    /// A field with no room for a tag column keeps the time. The time is the
    /// column the rest of the strip is read against, and a tag that pushed it
    /// out of one row would cost every row its anchor.
    #[test]
    fn a_field_too_narrow_for_a_tag_keeps_the_time() {
        let tagged = row(MINTED).tag("fix-auth").build();
        assert_eq!(
            tagged.label(9),
            "19-07-19 ",
            "eight columns and the gap leave a tag nothing",
        );
        assert_eq!(
            tagged.label(10),
            "19-07-19 \u{2026}",
            "one more column and the tag says it is there",
        );
        assert_eq!(
            tagged.label(4),
            row(MINTED).build().label(4),
            "and a field too narrow for the time itself cuts it the same way",
        );
        assert_eq!(tagged.label(0), "", "and nothing fits in nothing");
    }

    /// Whatever a field is handed, it comes out exactly as wide as it was
    /// budgeted, which is what keeps the separator in one column.
    #[test]
    fn a_field_is_exactly_as_wide_as_its_budget() {
        for text in [
            "",
            "short",
            "a-much-longer-label-than-fits",
            "会話ノート記録帳",
        ] {
            assert_eq!(
                gwidth(&field(text, 19), Method::Unicode),
                19,
                "{text:?} did not fill its field",
            );
        }
    }

    /// A header's rule reaches the field's edge, and the unreachable mark is set
    /// into the rule rather than hung off it.
    #[test]
    fn a_header_rules_out_to_the_field_edge() {
        assert_eq!(
            header_field("builder-1", false, 19),
            "builder-1 ".to_string() + &"─".repeat(9)
        );
        assert_eq!(
            header_field("laptop", true, 19),
            format!("laptop {}{UNREACHABLE_MARK}", "─".repeat(8)),
        );
        for (host, unreachable) in [("builder-1", false), ("a-very-long-host-name", true)] {
            assert_eq!(
                gwidth(&header_field(host, unreachable, 19), Method::Unicode),
                19,
                "{host:?} did not rule out to the edge",
            );
        }
    }

    /// The working set is legible: the three states get three brightnesses, and
    /// nothing else moves them (spec 9.2).
    #[test]
    fn the_working_set_shows_as_three_brightnesses() {
        assert_eq!(row("a").focused().build().presence(), Presence::Focused);
        assert_eq!(
            row("a").focused().attached().build().presence(),
            Presence::Focused,
            "the focused session is attached too, and focus is what it shows",
        );
        assert_eq!(row("a").attached().build().presence(), Presence::Background,);
        assert_eq!(row("a").build().presence(), Presence::Listed);
        // A working turn is the glyph's business, not the label's: a session
        // the client does not hold open stays dim while it runs.
        assert_eq!(
            row("a").status(RowStatus::Working).build().presence(),
            Presence::Listed,
        );
        assert_eq!(
            row("a")
                .attached()
                .status(RowStatus::Working)
                .build()
                .presence(),
            Presence::Background,
        );
        // An attachment the client holds is an attachment whatever the peer
        // can reach: reachability is the glyph's axis, and dimming the row
        // for it would say the client had let the session go.
        let out = row("a").attached().status(RowStatus::Unreachable).build();
        assert_eq!(out.presence(), Presence::Background);
        assert_eq!(out.status.glyph(), "!", "and the glyph is what says it");
    }

    /// A row on a host that has gone out keeps the brightness of what the
    /// client holds and wears the error glyph, and its header still carries
    /// the mark. Two independent axes, drawn at once.
    #[test]
    fn an_unreachable_row_keeps_its_working_set_brightness() {
        let rows = vec![
            row("s-1")
                .host("laptop")
                .attached()
                .status(RowStatus::Unreachable)
                .build(),
            row("s-2").host("builder-1").attached().build(),
        ];
        let cells = painted_cells(rows, 6);
        let text = |line: usize| -> String {
            cells[line]
                .iter()
                .map(|cell| cell.char.grapheme())
                .collect()
        };
        assert_eq!(text(0), " ~ laptop ──────── ! ─ │", "the header is marked");
        assert_eq!(text(1), " ! s-1                 │", "and so is the row");
        assert_eq!(text(2), " ~ builder-1 ───────── │");
        assert_eq!(text(3), "   s-2                 │");
        let styles = styles();
        assert_ne!(styles.text, styles.dim, "the two brightnesses differ");
        assert_eq!(
            cells[1][3].style, styles.text,
            "a row the client holds open is drawn as held open",
        );
        assert_eq!(
            cells[3][3].style, styles.text,
            "the same as one whose host is answering",
        );
    }

    fn styles() -> Rc<TranscriptStyles> {
        Rc::new(TranscriptStyles::from_theme(
            &aj_app::theme::Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
            crate::terminal::TerminalCaps::default(),
        ))
    }

    /// The tint the tests give the hover band: a color no other style in the
    /// strip carries, so a banded cell is unmistakable.
    const HOVER_BG: Color = Color::Rgb([9, 9, 9]);

    /// A drawn strip over `rows`, ready to be asked what a pointer lands on.
    fn strip(rows: Vec<SidebarRow>, height: u16) -> SessionSidebar {
        let state = Rc::new(RefCell::new(SidebarState {
            visible: true,
            rows,
            ..SidebarState::default()
        }));
        let mut strip = SessionSidebar::new(state, styles(), HOVER_BG);
        strip.draw(&crate::test_support::draw_ctx(SIDEBAR_COLS, Some(height)));
        strip
    }

    /// The strip's painted cells at its own width.
    fn painted_cells(rows: Vec<SidebarRow>, height: u16) -> Vec<Vec<vaxis::cell::Cell>> {
        let mut strip = strip(rows, height);
        let surface = strip.draw(&crate::test_support::draw_ctx(SIDEBAR_COLS, Some(height)));
        crate::test_support::flatten(&surface)
    }

    /// The strip's painted lines at its own width.
    fn painted(rows: Vec<SidebarRow>, height: u16) -> Vec<String> {
        painted_cells(rows, height)
            .iter()
            .map(|row| row.iter().map(|cell| cell.char.grapheme()).collect())
            .collect()
    }

    /// Every column of the strip, drawn: the focus marker left of the status
    /// glyph so the two never contend for one cell, the label field with the
    /// time of day in its leading column and a tag beside it, and the
    /// separator running the strip's full height whether or not there is a
    /// line beside it.
    #[test]
    fn the_strip_draws_its_columns() {
        let rows = vec![
            row("2026-08-06-19-07-19-368")
                .tag("fix-auth")
                .host("builder-1")
                .focused()
                .attached()
                .build(),
            row("s-2")
                .tag("eval-run")
                .host("builder-1")
                .status(RowStatus::Working)
                .build(),
            row("2026-08-06-18-40-49-001")
                .host("laptop")
                .status(RowStatus::Unreachable)
                .build(),
        ];
        assert_eq!(
            painted(rows, 7),
            vec![
                " ~ builder-1 ───────── │",
                "▌  19-07-19 fix-auth   │",
                " * s-2      eval-run   │",
                " ~ laptop ──────── ! ─ │",
                " ! 18-40-49            │",
                " + new                 │",
                "                       │",
            ],
        );
    }

    /// A focused row that is also working keeps both marks: the marker says
    /// where the user is, the glyph says what the session is doing. Sharing one
    /// column would cost whichever lost.
    #[test]
    fn a_focused_working_row_wears_both_marks() {
        let rows = vec![
            row("s-1")
                .tag("busy")
                .status(RowStatus::Working)
                .focused()
                .build(),
        ];
        assert_eq!(painted(rows, 2)[0], "▌* s-1      busy       │");
    }

    /// The glyph says what a session is doing and nothing else. Three rows all
    /// running a turn wear the same glyph in the same color whatever the client
    /// holds open, while their labels tell the three apart. Fold the two axes
    /// into one column and an unattached session running a turn stops reading
    /// as one.
    #[test]
    fn the_glyph_does_not_answer_for_the_working_set() {
        let rows = vec![
            row("s-1").status(RowStatus::Working).focused().build(),
            row("s-2").status(RowStatus::Working).attached().build(),
            row("s-3").status(RowStatus::Working).build(),
            row("s-4").status(RowStatus::Unseen).build(),
        ];
        let cells = painted_cells(rows, 5);
        let column = |col: usize| -> Vec<vaxis::cell::Style> {
            (0..4).map(|line| cells[line][col].style).collect()
        };
        let glyphs = column(1);
        assert_eq!(
            glyphs[..3],
            vec![glyphs[0]; 3],
            "one status is one glyph color, whoever holds the session open",
        );
        assert_ne!(
            glyphs[3], glyphs[0],
            "and the color is the status talking, not a constant: {glyphs:?}",
        );
        let labels = column(3);
        assert_ne!(labels[0], labels[1], "focused and background: {labels:?}");
        assert_ne!(labels[1], labels[2], "background and listed: {labels:?}");
        assert_ne!(labels[0], labels[2], "focused and listed: {labels:?}");
    }

    /// The label field answers the working-set question with one brightness
    /// across both its columns: the time and the tag alike take the row's own.
    /// A part of the field pinned to a style of its own would read as a second
    /// answer to a question the field already answers.
    ///
    /// The drawn cells also pin where the time column sits, which is the same
    /// eight columns on every row.
    #[test]
    fn the_label_field_carries_one_brightness() {
        let rows = vec![
            row("2026-08-06-19-07-19-368")
                .tag("on-screen")
                .focused()
                .build(),
            row("2026-08-06-18-40-49-001").tag("listed").build(),
        ];
        let cells = painted_cells(rows, 4);
        let styles = styles();
        // The field starts at column 3: marker, glyph, space. Its time takes
        // columns 3 to 10, the gap column 11, and the tag starts at 12.
        for (line, style, what) in [(0, styles.accent, "focused"), (1, styles.dim, "listed")] {
            assert_eq!(cells[line][3].style, style, "the {what} row's time");
            assert_eq!(cells[line][12].style, style, "and its tag");
        }
        for (line, expected) in [(0, "19-07-19"), (1, "18-40-49")] {
            let time: String = cells[line][3..11]
                .iter()
                .map(|cell| cell.char.grapheme())
                .collect();
            assert_eq!(time, expected, "line {line} drew the wrong eight columns");
        }
    }

    /// A drawn strip that records every gesture it resolves.
    fn wired(
        rows: Vec<SidebarRow>,
        height: u16,
    ) -> (SessionSidebar, Rc<RefCell<Vec<StripGesture>>>) {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut strip = strip(rows, height);
        let sink = Rc::clone(&seen);
        strip.set_on_gesture(Box::new(move |_, gesture| sink.borrow_mut().push(gesture)));
        (strip, seen)
    }

    fn report(line: i16, button: Button, kind: Type) -> Event {
        Event::Mouse(Mouse {
            col: 2,
            row: line,
            xoffset: 0,
            yoffset: 0,
            button,
            mods: vaxis::mouse::Modifiers::empty(),
            kind,
        })
    }

    fn press(line: i16) -> Event {
        report(line, Button::Left, Type::Press)
    }

    fn motion(line: i16) -> Event {
        report(line, Button::None, Type::Motion)
    }

    fn wheel(button: Button) -> Event {
        report(0, button, Type::Press)
    }

    /// Deliver `event` to the strip at target, returning the context so the
    /// caller can read what the strip asked the runtime for.
    fn deliver(strip: &mut SessionSidebar, event: &Event) -> EventContext {
        let mut ctx = EventContext::new();
        strip.handle_event(&mut ctx, event);
        ctx
    }

    /// The sessions the strip's last draw painted, in painted order.
    fn shown(strip: &SessionSidebar) -> Vec<String> {
        strip
            .gestures
            .iter()
            .filter_map(|gesture| match gesture {
                Some(StripGesture::Focus(id)) => Some(id.clone()),
                _ => None,
            })
            .collect()
    }

    fn redraw(strip: &mut SessionSidebar, height: u16) {
        strip.draw(&crate::test_support::draw_ctx(SIDEBAR_COLS, Some(height)));
    }

    fn focus_of(ids: &[&str]) -> Vec<StripGesture> {
        ids.iter()
            .map(|id| StripGesture::Focus((*id).to_string()))
            .collect()
    }

    /// A click resolves by the line it landed on, and a host header takes a
    /// line like every other: the strip's third line is the second session,
    /// not the third.
    #[test]
    fn a_click_names_the_session_on_the_line_it_landed_on() {
        // Two hosts, so the strip wears headers: `~ laptop`, its two rows,
        // `~ builder-1`, its two rows, then the create row.
        let rows = vec![
            row("laptop-new").host("laptop").focused().build(),
            row("builder-new").host("builder-1").build(),
            row("laptop-old").host("laptop").build(),
            row("builder-old").host("builder-1").build(),
        ];
        let (mut strip, seen) = wired(rows, 8);
        for line in [1, 2, 4, 5] {
            let ctx = deliver(&mut strip, &press(line));
            assert!(ctx.consume_event, "line {line} was acted on");
        }
        assert_eq!(
            *seen.borrow(),
            focus_of(&["laptop-new", "laptop-old", "builder-new", "builder-old"]),
            "the lines under the headers name the rows below them",
        );

        // Only the press acts. A drag crossing the strip belongs to whatever
        // started it, and a release would fire the row a second time.
        for kind in [Type::Drag, Type::Release] {
            deliver(&mut strip, &report(1, Button::Left, kind));
        }
        assert_eq!(seen.borrow().len(), 4, "only the presses acted");
    }

    /// A click answers with the session that was painted on the line, not with
    /// whatever the mirror holds by the time the press arrives.
    ///
    /// The drive loop refreshes the mirror at the top of every iteration and
    /// paints only once the frame budget has elapsed, so a press handled in
    /// between sees rows that have already moved. A background session going
    /// active reorders them, and a new row at the top shifts every index by
    /// one.
    #[test]
    fn a_click_names_the_session_that_was_painted_on_the_line() {
        let (mut strip, seen) = wired(rows_named(&["a", "b", "c"]), 8);
        assert_eq!(shown(&strip), ["a", "b", "c"], "painted in this order");

        // Refreshed with a newly active session at the top. No paint follows,
        // so the screen still shows `a` on the first line.
        strip.state.borrow_mut().rows = rows_named(&["new", "a", "b", "c"]);
        deliver(&mut strip, &press(0));
        assert_eq!(*seen.borrow(), focus_of(&["a"]));

        // And the next paint moves the answer along with the rows.
        redraw(&mut strip, 8);
        deliver(&mut strip, &press(0));
        assert_eq!(*seen.borrow(), focus_of(&["a", "new"]));
    }

    /// The same holds when the rows shrink under the strip: the line still
    /// names the session that was drawn on it, rather than resolving its
    /// index against a shorter list or falling off the end of one.
    #[test]
    fn a_click_after_the_rows_shrink_still_names_what_was_painted() {
        let (mut strip, seen) = wired(rows_named(&["a", "b", "c"]), 8);
        strip.state.borrow_mut().rows = rows_named(&["a"]);
        let ctx = deliver(&mut strip, &press(2));
        assert_eq!(*seen.borrow(), focus_of(&["c"]));
        assert!(ctx.consume_event, "and the strip acted on the press");
    }

    /// The create row asks for a session, from wherever the layout put it.
    #[test]
    fn a_click_on_the_create_row_asks_for_a_session() {
        let (mut strip, seen) = wired(rows_named(&["a", "b"]), 8);
        let ctx = deliver(&mut strip, &press(2));
        assert_eq!(*seen.borrow(), vec![StripGesture::New]);
        assert!(ctx.consume_event);
    }

    /// A header and the overflow count say something rather than offer
    /// something, so a click on either does nothing at all and leaves the
    /// press to whatever else wants it.
    #[test]
    fn a_header_and_the_overflow_count_are_not_affordances() {
        let grouped = vec![row("a").host("one").build(), row("b").host("two").build()];
        let (mut strip, seen) = wired(grouped, 8);
        let ctx = deliver(&mut strip, &press(0));
        assert!(seen.borrow().is_empty(), "a header click does nothing");
        assert!(!ctx.consume_event, "and does not swallow the press");

        // Five rows in five lines: three rows, the overflow count, the create
        // row.
        let (mut strip, seen) = wired(rows_named(&["a", "b", "c", "d", "e"]), 5);
        let ctx = deliver(&mut strip, &press(3));
        assert!(seen.borrow().is_empty(), "an overflow click does nothing");
        assert!(!ctx.consume_event);
    }

    /// The lines below the strip's content are not rows. The layout leaves
    /// them out of its result, so they resolve to nothing by construction.
    #[test]
    fn a_click_below_the_content_lands_on_nothing() {
        let (mut strip, seen) = wired(rows_named(&["a", "b"]), 8);
        for line in [3, 7, 40] {
            deliver(&mut strip, &press(line));
        }
        assert!(seen.borrow().is_empty());
    }

    /// The background of each painted line's first label cell.
    fn painted_bgs(strip: &mut SessionSidebar, height: u16) -> Vec<Color> {
        let surface = strip.draw(&crate::test_support::draw_ctx(SIDEBAR_COLS, Some(height)));
        crate::test_support::flatten(&surface)
            .iter()
            .map(|row| row[3].style.bg)
            .collect()
    }

    fn banded(bgs: &[Color]) -> Vec<usize> {
        bgs.iter()
            .enumerate()
            .filter(|(_, bg)| **bg == HOVER_BG)
            .map(|(line, _)| line)
            .collect()
    }

    /// The row under the pointer wears a band, and the pointer leaving takes
    /// the band with it.
    #[test]
    fn the_pointer_bands_the_row_under_it() {
        let (mut strip, _) = wired(rows_named(&["a", "b", "c"]), 6);
        let ctx = deliver(&mut strip, &motion(1));
        assert!(ctx.redraw, "the band moved, so the frame is dirty");
        let bgs = painted_bgs(&mut strip, 6);
        assert_eq!(banded(&bgs), vec![1], "exactly the hovered row: {bgs:?}");

        let ctx = deliver(&mut strip, &Event::MouseLeave);
        assert!(ctx.redraw);
        let bgs = painted_bgs(&mut strip, 6);
        assert!(banded(&bgs).is_empty(), "the band left with it: {bgs:?}");
    }

    /// The band marks what a click acts on and nothing else. Over a header,
    /// the overflow count or the blank below the strip it would offer
    /// something that is not there.
    #[test]
    fn the_band_only_marks_what_a_click_acts_on() {
        // Five lines: `~ one`, a, the overflow count, the create row, and one
        // blank below the content.
        let rows = vec![
            row("a").host("one").build(),
            row("b").host("two").build(),
            row("c").host("two").build(),
        ];
        let (mut strip, _) = wired(rows, 5);
        for line in [0, 2, 4, 20] {
            deliver(&mut strip, &motion(line));
            let bgs = painted_bgs(&mut strip, 5);
            assert!(
                banded(&bgs).is_empty(),
                "line {line} is not an affordance: {bgs:?}",
            );
        }
        deliver(&mut strip, &motion(3));
        let bgs = painted_bgs(&mut strip, 5);
        assert_eq!(banded(&bgs), vec![3], "but the create row is: {bgs:?}");
        deliver(&mut strip, &motion(1));
        let bgs = painted_bgs(&mut strip, 5);
        assert_eq!(banded(&bgs), vec![1], "and so is the row: {bgs:?}");
    }

    /// The band asks for a frame when it moves and never otherwise. A pointer
    /// resting on the strip reports on every terminal event, and a hover that
    /// redrew for each of them would keep the frame loop awake for nothing.
    #[test]
    fn a_pointer_holding_still_asks_for_no_frame() {
        let (mut strip, _) = wired(rows_named(&["a", "b", "c"]), 6);
        assert!(deliver(&mut strip, &motion(1)).redraw);
        assert!(
            !deliver(&mut strip, &motion(1)).redraw,
            "the same row again changed nothing",
        );
        assert!(deliver(&mut strip, &Event::MouseLeave).redraw);
        assert!(
            !deliver(&mut strip, &Event::MouseLeave).redraw,
            "and a second leave has nothing left to clear",
        );
    }

    /// The rows can move under a pointer that has not, and the band goes by
    /// what is on the line now, not by what was there when the pointer landed.
    #[test]
    fn the_band_follows_the_rows_moving_under_it() {
        let (mut strip, _) = wired(rows_named(&["a", "b", "c"]), 6);
        deliver(&mut strip, &motion(1));
        assert_eq!(banded(&painted_bgs(&mut strip, 6)), vec![1]);

        // A host appears on the second row, so line 1 is now its header: rows
        // with no host sort above every header.
        strip.state.borrow_mut().rows = vec![row("a").build(), row("b").host("builder-1").build()];
        let bgs = painted_bgs(&mut strip, 6);
        assert!(
            banded(&bgs).is_empty(),
            "the band came off a line that is no longer a row: {bgs:?}",
        );
    }

    /// The strip reaches the capturing phase only when something floats above
    /// it, and that gesture is not the strip's. It drops its band rather than
    /// pointing at a row the click will never reach.
    #[test]
    fn a_gesture_that_belongs_to_something_above_clears_the_band() {
        let (mut strip, seen) = wired(rows_named(&["a", "b", "c"]), 6);
        deliver(&mut strip, &motion(1));
        assert_eq!(banded(&painted_bgs(&mut strip, 6)), vec![1]);

        let mut ctx = EventContext::new();
        strip.capture_event(&mut ctx, &press(1));
        assert!(ctx.redraw, "the band cleared, so the frame is dirty");
        assert!(
            seen.borrow().is_empty(),
            "and the press was not the strip's"
        );
        assert!(banded(&painted_bgs(&mut strip, 6)).is_empty());
    }

    /// The wheel scrolls a strip that cannot show every row, and stops once
    /// the run reaches an end rather than sliding the content off screen.
    ///
    /// A wheel report carries a position, so it also moves the hover band.
    /// What says the run moved is the consume, which only a scroll that landed
    /// somewhere new asks for.
    #[test]
    fn the_wheel_scrolls_a_strip_that_overflows() {
        let ids: Vec<String> = (0..10).map(|i| format!("s-{i}")).collect();
        let rows: Vec<SidebarRow> = ids.iter().map(|id| row(id).build()).collect();
        // Six lines: four rows, the overflow count, the create row.
        let (mut strip, _) = wired(rows, 6);
        assert_eq!(shown(&strip), ["s-0", "s-1", "s-2", "s-3"]);

        let ctx = deliver(&mut strip, &wheel(Button::WheelDown));
        assert!(ctx.consume_event && ctx.redraw);
        redraw(&mut strip, 6);
        assert_eq!(shown(&strip), ["s-1", "s-2", "s-3", "s-4"]);

        deliver(&mut strip, &wheel(Button::WheelUp));
        redraw(&mut strip, 6);
        assert_eq!(shown(&strip), ["s-0", "s-1", "s-2", "s-3"]);

        // Off the bottom: the run stops on the last row.
        for _ in 0..20 {
            deliver(&mut strip, &wheel(Button::WheelDown));
            redraw(&mut strip, 6);
        }
        assert_eq!(shown(&strip), ["s-6", "s-7", "s-8", "s-9"]);
        assert!(
            !deliver(&mut strip, &wheel(Button::WheelDown)).consume_event,
            "and the wheel has nothing left to do",
        );
    }

    /// A strip whose rows all fit has nowhere to scroll, and the wheel leaves
    /// it exactly as it was rather than quietly anchoring the run where it
    /// already sits (which would switch focus-following off for nothing).
    #[test]
    fn the_wheel_is_inert_while_the_rows_fit() {
        let mut rows = rows_named(&["a", "b", "c"]);
        rows[2].focused = true;
        let (mut strip, _) = wired(rows, 10);
        for button in [Button::WheelDown, Button::WheelUp] {
            let ctx = deliver(&mut strip, &wheel(button));
            assert!(!ctx.consume_event, "{button:?} had nowhere to go");
        }
        redraw(&mut strip, 10);
        assert_eq!(shown(&strip), ["a", "b", "c"]);
    }

    /// The wheel moves the run from wherever it already sits, which is what
    /// keeps it in step with the layout's own focus-following rather than
    /// re-deriving a top of its own.
    #[test]
    fn the_wheel_scrolls_from_where_the_run_already_sits() {
        let ids: Vec<String> = (0..10).map(|i| format!("s-{i}")).collect();
        let mut rows: Vec<SidebarRow> = ids.iter().map(|id| row(id).build()).collect();
        rows[9].focused = true;
        let (mut strip, _) = wired(rows, 6);
        assert_eq!(
            shown(&strip),
            ["s-6", "s-7", "s-8", "s-9"],
            "the layout put the run at the bottom to keep focus on screen",
        );
        deliver(&mut strip, &wheel(Button::WheelUp));
        redraw(&mut strip, 6);
        assert_eq!(
            shown(&strip),
            ["s-5", "s-6", "s-7", "s-8"],
            "one row up from there, not one row down from the top",
        );
    }

    /// A focus change drops the wheel's anchor, so switching sessions always
    /// brings the focused row back into view. A refresh that leaves focus
    /// where it was leaves the user's scroll alone.
    ///
    /// The rows go straight into the mirror, the way the drive loop's refresh
    /// puts them there: the rule is a property of the rows on hand, not of
    /// anyone remembering to route the write through a setter.
    #[test]
    fn a_focus_change_pulls_the_view_back_to_the_focused_row() {
        let ids: Vec<String> = (0..10).map(|i| format!("s-{i}")).collect();
        let build = |focused: Option<usize>| -> Vec<SidebarRow> {
            ids.iter()
                .enumerate()
                .map(|(at, id)| {
                    let built = row(id);
                    if Some(at) == focused {
                        built.focused()
                    } else {
                        built
                    }
                    .build()
                })
                .collect()
        };
        let (mut strip, _) = wired(build(None), 6);
        for _ in 0..3 {
            deliver(&mut strip, &wheel(Button::WheelDown));
        }
        redraw(&mut strip, 6);
        assert_eq!(shown(&strip), ["s-3", "s-4", "s-5", "s-6"]);

        strip.state.borrow_mut().rows = build(None);
        redraw(&mut strip, 6);
        assert_eq!(
            shown(&strip),
            ["s-3", "s-4", "s-5", "s-6"],
            "a mirror refresh that does not move focus leaves the scroll alone",
        );

        strip.state.borrow_mut().rows = build(Some(9));
        redraw(&mut strip, 6);
        assert_eq!(
            shown(&strip),
            ["s-6", "s-7", "s-8", "s-9"],
            "and a focus change hands the strip back to focus-following",
        );
    }
}
