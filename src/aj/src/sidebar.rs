//! The session sidebar: a strip listing the sessions the peer offers that the
//! user has not put away (spec 9.2).
//!
//! The widget is read-only chrome. It renders from a [`SidebarState`] mirror
//! the drive loop refreshes once per iteration from the client's session
//! directory, the same single-writer arrangement the status chrome uses. The
//! widget cannot reach the directory itself (that lives behind the world), and
//! mirroring keeps the draw path free of any borrow the loop also wants.
//!
//! The strip is an orientation instrument, not a store browser. That is what
//! decides its behavior: the drawn window follows the focused row so focus is
//! never off screen, the stepping chords walk the order the user can see, and
//! the session selector is the browser.
//!
//! Activity decides what is visible, never where it sits. Groups sit in the
//! order their labels sort and rows sit where their group put them, whatever
//! happens on them, because a row that moves between the look and the click is
//! a row the pointer misses. Recency still decides which rows survive the
//! per-group cap (see [`GROUP_CAP`]), and what moved is carried by the glyphs,
//! which is a signal that costs a row no movement.
//!
//! A row answers four independent questions, and each gets exactly one
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
//! - Has the user put it away: a strike through the label field, which is only
//!   ever seen on a revealed row or on one the working set is holding open
//!   (see [`SidebarRow::put_away`]). A strike rather than a brightness,
//!   because brightness is spoken for and an archived row the client holds
//!   open has to answer both questions at once.
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
//! arithmetic (host headers, the per-group fold lines, the overflow row and
//! the create row all take lines away from the rows) therefore lives in one
//! testable place.
//!
//! Pointer gestures are a second trigger for actions the chords already
//! dispatch, never a behavior of their own (spec 9.2). The strip resolves a
//! click into a [`StripGesture`], which names a session, a group to fold, or
//! a create and nothing else, and the shell hands that to the same place the
//! chord's handler hands its own answer. A draw records what each line it paints
//! resolves to, so a click answers with the session the user is looking at
//! even when the mirror has moved since (see [`SessionSidebar::gestures`]).

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use aj_wire::{DirectoryHost, SessionSummary};
use chrono::{DateTime, Utc};
use vaxis::cell::{Color, Style};
use vaxis::gwidth::{Method, gwidth};
use vaxis::mouse::{Button, Mouse, Type};
use vaxis::vxfw::{
    DrawContext, Event, EventContext, MaxSize, Overflow, RichText, Size, Surface, TextSpan, Widget,
};

use crate::text::one_line;
use crate::transcript::TranscriptStyles;

/// Columns the sidebar occupies when shown, until `sidebar_cols` says
/// otherwise.
///
/// Fixed rather than proportional: a strip that grew with the terminal would
/// take width from the transcript for nothing. That argument is about the
/// terminal's width, not the user's taste, which is why the width is a
/// setting and not a fraction. The columns go one to the focus marker, one to
/// the status glyph, one to a space, the rest to the label, and the last two
/// to a pad and the separator rule.
pub(crate) const SIDEBAR_COLS: u16 = aj_conf::DEFAULT_SIDEBAR_COLS;

/// Columns a transcript needs beside the strip to be worth reading: about a
/// short line of prose.
const MIN_TRANSCRIPT_COLS: u16 = 20;

/// Terminal width below which the strip holds itself back.
///
/// The strip is inflexible, so under this it would leave the transcript
/// beside it too little to read. A function of the strip's own width rather
/// than a constant: a configured strip wide enough to swallow an 80-column
/// terminal has to suppress itself there, and `FlexRow` hands the flexible
/// child `max.width - first_pass_width`, so the transcript would otherwise
/// get zero columns.
pub(crate) fn min_cols_with_sidebar(cols: u16) -> u16 {
    cols.saturating_add(MIN_TRANSCRIPT_COLS)
}

/// The focused row's marker, in the column left of the status glyph.
const FOCUS_MARKER: &str = "▌";

/// The glyph a folded group's trailing line wears, pointing right at the count
/// that stands in for the rows the cap holds back.
const FOLDED_MARKER: &str = "▸";

/// The glyph an unfolded group's trailing line wears, pointing up at the rows
/// it revealed. Clicking it folds them away again.
const UNFOLDED_MARKER: &str = "▴";

/// Boring rows a group shows before the rest fold behind its trailing line.
///
/// Boring is [`SidebarRow::boring`]: the rows the cap is allowed to hold back.
/// The exempt ones are usually the recent ones anyway, so the exemptions
/// rarely add a line.
///
/// Five, because three hosts then fit a laptop-height pane with room to spare,
/// which is the case that made the strip unreadable: one host with two dozen
/// idle sessions buried every other host. The cost is that a host's deep tail
/// is one gesture away rather than in view, which is the fold line's whole
/// job. A constant rather than a setting: a number the user has to tune is a
/// design that has not been made.
const GROUP_CAP: usize = 5;

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
    /// Whether the user has put the session away (spec 6.8).
    pub(crate) archived: bool,
    /// When the peer says the session last did something, on the peer's clock.
    ///
    /// Nothing places a row by it (see the module doc). It is what the
    /// per-group cap selects on: which rows a group keeps is a question about
    /// activity, where it keeps them is not (see [`GROUP_CAP`]).
    pub(crate) last_activity: DateTime<Utc>,
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

    /// Whether the per-group cap may hold this row back (see [`GROUP_CAP`]).
    ///
    /// The four exemptions are the strip's standing promises, read off the two
    /// axes a row already encodes: the working set (focused, attached) and
    /// what the session is doing (working, unseen output).
    ///
    /// A row on a host the peer cannot reach is boring, which is deliberate:
    /// its glyph says the peer cannot answer for it at all, so there is no
    /// working or attention signal left to suppress, and a host that has gone
    /// out with two dozen sessions on it is exactly the crowd the cap exists
    /// to bound. Holding it open still exempts it, like any other row.
    fn boring(&self) -> bool {
        !self.focused
            && !self.attached
            && !matches!(self.status, RowStatus::Working | RowStatus::Unseen)
    }

    /// Whether the strip leaves this row out of the default view.
    ///
    /// Archiving is the user saying they are done with a session, so the row
    /// goes. The exemption is the working set, which is the cap's first two
    /// and for the same reason: a row the client holds open is part of what
    /// the user is working on right now, and hiding one would leave the strip
    /// describing a working set the user cannot see. Focus is named beside
    /// attachment for the reader, though the focused session is always in the
    /// working set, so it never decides this on its own.
    ///
    /// What the session is doing exempts nothing here. The cap suppresses a
    /// row the user did not ask about, this hides one they asked to put away,
    /// and a turn running inside it does not undo the asking.
    fn put_away(&self) -> bool {
        self.archived && !self.focused && !self.attached
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
        let session = self.label_source();
        let Some(tag) = &self.tag else {
            return field(&session_label(session, cols), cols);
        };
        let id_cols = cols.min(ID_COLS);
        let id = session_label(session, id_cols);
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

    /// What this row's label is read from: its id with the qualifier its `host`
    /// field names stripped off the front, and the whole id when the row
    /// carries no host, is not qualified with it, or has nothing left after it.
    ///
    /// Not an id. A gateway resolves the qualified form and nothing else, so
    /// everything that addresses the session uses `id` as it arrived.
    ///
    /// The qualifier is matched from the host the row carries into the id,
    /// never read out of the id: a client is told which host a row belongs to
    /// precisely so it does not have to parse an id it is not allowed to parse
    /// (spec 6.2 goes further than the direction rule, "clients never parse
    /// session ids", which is why this matches a string it was handed rather
    /// than looking for a separator). See [`aj_wire::SessionSummary::host`].
    ///
    /// One qualifier deep, which is what a gateway writes (the separator is the
    /// wire's, written down in full in `gateway::naming`). A peer that
    /// qualifies some other way, or a gateway fronting a gateway, keeps the
    /// label it has today rather than losing a guessed-at slice of it.
    fn label_source(&self) -> &str {
        session_label_source(&self.id, self.host.as_deref())
    }
}

/// The groups the user unfolded, keyed as [`Group::key`] keys them: the host id
/// a group's rows carry, and not the text its header draws.
///
/// A header's text is not identity. Keying this by the label would fold a host
/// under one name and leave it unfoldable under the next one it reports.
///
/// `None` is the unlabeled group a plain host's rows sit in, which the cap
/// bounds like any other and which therefore folds like any other.
///
/// Client state and nothing else: it outlives every refresh of the mirror,
/// lapses with the process, and is never sent anywhere. Held as a list because
/// it holds one entry per host the user has opened up, which is a handful.
#[derive(Default)]
struct Unfolded(Vec<Option<String>>);

impl Unfolded {
    /// Whether `group` is unfolded, so its rows escape the cap.
    fn holds(&self, group: Option<&str>) -> bool {
        self.0.iter().any(|held| held.as_deref() == group)
    }

    /// Unfold `group` if it is folded, fold it if it is not.
    fn toggle(&mut self, group: Option<&str>) {
        match self.0.iter().position(|held| held.as_deref() == group) {
            Some(at) => {
                self.0.swap_remove(at);
            }
            None => self.0.push(group.map(str::to_string)),
        }
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
    /// The configured width, once the drive loop has read it out of the
    /// config. `None` until then, and in any test that does not set one.
    configured_cols: Option<u16>,
    /// Rows in display order (see [`rows_for_display`]).
    pub(crate) rows: Vec<SidebarRow>,
    /// The hosts the peer named alongside its rows, empty against a plain host
    /// (spec 7.1).
    ///
    /// Held rather than read back off the rows, because the entry that matters
    /// is the one for a host the peer holds no rows for: that host appears
    /// nowhere else, and it is the one the strip would otherwise draw as
    /// nothing at all.
    pub(crate) hosts: Vec<DirectoryHost>,
    /// The groups the user opened past the cap.
    unfolded: Unfolded,
    /// Whether the strip shows the sessions the user has put away.
    ///
    /// Client state like [`Unfolded`]: it outlives every refresh of the
    /// mirror, lapses with the process, and is never sent anywhere. Off by
    /// default, which is the whole point of archiving one.
    pub(crate) reveal_archived: bool,
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

    /// Columns the strip draws in.
    ///
    /// [`SIDEBAR_COLS`] until the loop has read the setting, so a strip
    /// nobody configured is the strip the app ships.
    pub(crate) fn cols(&self) -> u16 {
        self.configured_cols.unwrap_or(SIDEBAR_COLS)
    }

    /// Take the configured width, which the drive loop reads off the live
    /// config once per iteration ([`crate::interactive::sync_sidebar`]).
    pub(crate) fn set_cols(&mut self, cols: u16) {
        self.configured_cols = Some(cols);
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
        strip_lines(
            &self.rows,
            &self.hosts,
            &self.unfolded,
            height,
            self.anchor(),
        )
    }

    /// Fold `group` if it is unfolded, unfold it if it is not, so the cap
    /// either holds its boring rows back or shows all of them.
    ///
    /// `group` is the host id a group's rows carry ([`Group::key`]), `None` the
    /// unlabeled group a plain host's rows sit in. Naming the group rather than
    /// a row is what lets the pointer undo what the chord did and the other way
    /// round: both name the same thing. The id and not the header's text,
    /// because a host that renames itself would otherwise fold twice and unfold
    /// neither.
    pub(crate) fn toggle_fold(&mut self, group: Option<&str>) {
        self.unfolded.toggle(group);
        // The wheel's anchor is an index into a display order this just
        // changed, so keeping it would leave the user looking at rows they did
        // not ask for, and an unfold whose rows all land above the anchor
        // would look like a gesture that did nothing at all. Dropping it hands
        // the strip back to focus-following, the same answer a focus change
        // gets (see [`Anchor`]).
        self.scroll = None;
    }

    /// Fold or unfold the group the focused row sits in, and say whether there
    /// was one to work on.
    ///
    /// The keyboard's half of the fold gesture. It reads the group off the
    /// focused row because that is the row the user is looking at, and a
    /// chord that asked which host it meant would be a second selection.
    pub(crate) fn toggle_focused_fold(&mut self) -> bool {
        let Some(group) = self
            .rows
            .iter()
            .find(|row| row.focused)
            .map(|row| row.host.clone())
        else {
            return false;
        };
        self.toggle_fold(group.as_deref());
        true
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
        let layout = Layout::of(&self.rows, &self.hosts, &self.unfolded);
        // The same division the draw lays out under, so the wheel cannot
        // anchor the run somewhere the draw would not put it (see
        // [`Layout::split`]).
        let budget = layout.split(height).rows;
        if budget == 0 {
            return false;
        }
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
/// Ordered by id descending, which is a stable key and, on the ids the app
/// mints, puts the newest session at the top of its group. Nothing here reads
/// activity: a row that moved out from under the pointer between the look and
/// the click is a row the user cannot hit, so the strip holds still and lets
/// the glyphs carry what changed (spec 9.2, and see the module doc). The
/// per-group cap is where recency is still read (see [`GROUP_CAP`]).
///
/// `unseen` answers spec 6.8's "has it moved since I looked" for a row the
/// caller already holds, and `attached` answers "do I hold it open" for a
/// session id, which keeps this linear.
///
/// Archived rows are left out unless `reveal_archived`, which is the toggle
/// that shows them inline. They never enter the row set, so no cap counts one
/// and neither does the row count that decides whether the strip shows at
/// all. See [`SidebarRow::put_away`] for what stays regardless.
pub(crate) fn rows_for_display(
    rows: &[SessionSummary],
    focused: &str,
    unseen: impl Fn(&SessionSummary) -> bool,
    attached: impl Fn(&str) -> bool,
    reveal_archived: bool,
) -> Vec<SidebarRow> {
    let mut ordered: Vec<&SessionSummary> = rows.iter().collect();
    ordered.sort_by(|l, r| r.id.cmp(&l.id));
    ordered
        .into_iter()
        .map(|row| SidebarRow {
            status: RowStatus::of(row, unseen(row)),
            focused: row.id == focused,
            attached: attached(&row.id),
            archived: row.archived,
            tag: row.tag.clone(),
            host: row.host.clone(),
            id: row.id.clone(),
            last_activity: row.last_activity,
        })
        .filter(|row| reveal_archived || !row.put_away())
        .collect()
}

/// The session a next/previous step lands on, walking the displayed order and
/// wrapping at the ends.
///
/// The displayed order is the layout's, so a step skips the rows the cap holds
/// back and crosses group boundaries where the strip does. That is the point:
/// stepping is orientation across hosts, and a step that walked the hidden
/// rows would attach a stale session per press without showing anything (spec
/// 9.2). A host's tail is reached by unfolding it or by opening the session
/// selector, whose connected row source is not subject to the strip's cap.
///
/// `None` when there is nothing to move to: fewer than two displayed rows, or
/// no row claiming focus (the directory and the rows disagree, so any answer
/// would be a guess). A focused row is never one the cap holds back, so it is
/// always in the order this walks.
pub(crate) fn step_session(state: &SidebarState, forward: bool) -> Option<String> {
    let layout = Layout::of(&state.rows, &state.hosts, &state.unfolded);
    let order = &layout.order;
    if order.len() < 2 {
        return None;
    }
    let at = order.iter().position(|&index| state.rows[index].focused)?;
    let next = if forward {
        (at + 1) % order.len()
    } else {
        (at + order.len() - 1) % order.len()
    };
    Some(state.rows[order[next]].id.clone())
}

/// One drawn line of the strip.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum StripLine {
    /// A host's group header, drawn above that host's rows.
    Header {
        /// What the header reads, as [`Group::label`] draws it and not as its
        /// group is keyed: this is text for a reader, and nothing resolves a
        /// host by it.
        label: String,
        /// Whether the peer can reach none of the host's rows.
        unreachable: bool,
    },
    /// A session row, named by its index into the rows the layout was built
    /// from, so a caller resolves it without re-deriving the display order.
    Session { index: usize },
    /// A group's trailing fold line, the affordance that opens the group past
    /// the cap and closes it again (see [`GROUP_CAP`]).
    Fold {
        /// The group this line belongs to, keyed as [`Group::key`] keys it and
        /// not as its header reads: a click here folds what the chord folds
        /// (see [`SidebarState::toggle_fold`]). `None` is the unlabeled group a
        /// plain host's rows sit in.
        host: Option<String>,
        /// How many of the group's rows the cap holds back. Zero exactly when
        /// the group is unfolded, and the line is then what folds it again.
        hidden: usize,
    },
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
    /// Fold or unfold the group whose fold line was clicked, `None` being the
    /// unlabeled group (see [`SidebarState::toggle_fold`]).
    Fold(Option<String>),
    /// Create a session.
    New,
}

/// Lay the strip out for `rows` in `height` lines.
///
/// The one place the height arithmetic lives. Host headers, the fold lines,
/// the overflow row and the create row all take lines away from the rows, and
/// the focused row has to survive that. Never returns more than `height`
/// lines.
///
/// `hosts` is what the peer says its directory is made of, which is where a
/// host it holds no rows for comes from: that host is drawn as an empty group,
/// a header with nothing under it (see [`Layout::empty_headers`]).
///
/// `unfolded` names the groups that show every row rather than the cap's share
/// of them (see [`GROUP_CAP`]).
///
/// `scroll` is where the wheel anchored the run, or `None` to follow the
/// focused row (see [`SidebarState::scroll_by`]).
///
/// A height that cannot fit even the focused row and the chrome of its group
/// (a header, a fold line) gives up on showing a row rather than overrunning.
/// What is left at that size is the overflow count, which still says how many
/// rows are there to be seen.
fn strip_lines(
    rows: &[SidebarRow],
    hosts: &[DirectoryHost],
    unfolded: &Unfolded,
    height: u16,
    scroll: Option<usize>,
) -> Vec<StripLine> {
    if height == 0 {
        return Vec::new();
    }
    let layout = Layout::of(rows, hosts, unfolded);
    let budget = layout.split(height);
    layout.lines(layout.visible_run(budget.rows, scroll), budget)
}

/// A run of rows sharing a host, or a host the peer holds no rows for.
struct Group<'a> {
    /// The host this group is, as its rows name it: the id its sessions are
    /// namespaced under, or the address for a host the peer has no id for.
    /// `None` on rows from a plain host, which are all its own and are grouped
    /// under no name.
    ///
    /// The group's identity rather than its text, which is why a fold is
    /// remembered by it: a host that changes what it calls itself must not
    /// unfold as one group and fold as another.
    key: Option<&'a str>,
    /// What the header draws for it (see [`host_label`]). `None` exactly where
    /// the key is, both being held to the same rule about what names something
    /// (see [`named`]): a headerless run belongs to no host by name.
    label: Option<&'a str>,
    /// Whether the peer can reach none of it, which the header says once
    /// instead of the user reading it off every row.
    unreachable: bool,
    /// Where the group's rows sit in [`Layout::order`]. Empty for a host the
    /// peer holds no rows for (spec 7.1).
    span: Range<usize>,
    /// How many of the group's rows the cap holds back, zero when it holds
    /// back none (see [`GROUP_CAP`]).
    hidden: usize,
    /// Whether the user opened this group past the cap.
    unfolded: bool,
}

/// What the header draws for the host `key` names: the name the peer publishes
/// for it, else `key` itself.
///
/// A group is keyed on what its rows say (an id), and a name arrives on the
/// peer's own entry for that host, so the two are joined here. A key the peer
/// publishes no entry for keeps labelling itself, which is what a client mid
/// refresh has.
fn labelled<'a>(hosts: &'a [DirectoryHost], key: &'a str) -> &'a str {
    hosts
        .iter()
        .find(|host| host.id.as_deref() == Some(key))
        .and_then(host_label)
        .unwrap_or(key)
}

/// The rows of one group that the cap holds back: its boring rows past the
/// [`GROUP_CAP`] most recently active of them, as indices into `rows`.
fn held_back(rows: &[SidebarRow], members: &[usize]) -> Vec<usize> {
    let mut boring: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&at| rows[at].boring())
        .collect();
    // A copy is sorted, never the group: recency picks which rows survive and
    // nothing else in the strip reads it, so the survivors stay exactly where
    // the group had them. The sort is stable, so rows sharing a stamp are cut
    // from the tail of the display order rather than arbitrarily.
    boring.sort_by_key(|&at| std::cmp::Reverse(rows[at].last_activity));
    boring.split_off(boring.len().min(GROUP_CAP))
}

/// The display order of the rows and the host groups over it.
struct Layout<'a> {
    rows: &'a [SidebarRow],
    /// Row indices in display order: the groups in label order, each holding
    /// the rows the cap left it, in the order they arrived.
    order: Vec<usize>,
    /// The groups, each a contiguous span of [`Self::order`]. A host the peer
    /// holds no rows for is a group whose span is empty, sitting where its
    /// label sorts.
    groups: Vec<Group<'a>>,
    /// Whether groups wear headers at all. One host, or none, is not a
    /// grouping: a plain single-host connect has to look exactly as it would
    /// have before hosts existed.
    ///
    /// A group with no rows is not subject to this: it is its header and
    /// nothing else (see [`Layout::empty_headers`]).
    headed: bool,
}

/// How a strip's height divides between the hosts the peer holds no rows for
/// and the rows (see [`Layout::split`]).
struct Budget {
    /// How many of those hosts' headers are drawn.
    empty: usize,
    /// Lines left for the rows, the headers and fold lines over them, and the
    /// count of the ones that did not fit.
    rows: usize,
}

impl<'a> Layout<'a> {
    fn of(rows: &'a [SidebarRow], hosts: &'a [DirectoryHost], unfolded: &Unfolded) -> Self {
        // Rows gathered under the host they name, keeping the order they
        // arrived in, which is the display order (see [`rows_for_display`]).
        let mut gathered: Vec<(Option<&str>, Vec<usize>)> = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            let host = row.host.as_deref();
            match gathered.iter_mut().find(|(key, _)| *key == host) {
                Some((_, members)) => members.push(index),
                None => gathered.push((host, vec![index])),
            }
        }
        let mut groups: Vec<Group<'a>> = gathered
            .iter()
            .map(|&(key, _)| Group {
                key,
                // The rows carry the id and the peer's own entry carries the
                // name, so the label is looked up there rather than read off a
                // row.
                label: key.map(|key| labelled(hosts, key)),
                // Refined against the group's own rows below. A group built
                // from rows always has some.
                unreachable: true,
                span: 0..0,
                hidden: 0,
                unfolded: unfolded.holds(key),
            })
            .collect();
        // Then the hosts the peer holds no rows for, which no scan over the
        // rows could have found (spec 7.1).
        groups.extend(
            hosts
                .iter()
                .filter(|host| {
                    // A host whose rows we hold is already a group, named by the
                    // id those rows carry. An address never matches one: rows
                    // are namespaced by id (spec 6.2), so a host with no id has
                    // no rows here either.
                    host.id
                        .as_deref()
                        .is_none_or(|id| !gathered.iter().any(|&(key, _)| key == Some(id)))
                })
                .filter_map(|host| {
                    // The learned id where the peer has one, the configured
                    // address until it does (spec 7.1). An entry naming neither
                    // is not a group: it can hold no rows, and a header keyed on
                    // nothing would fold the plain host's unlabeled run.
                    let key = named(&host.id).or_else(|| named(&host.address))?;
                    Some(Group {
                        key: Some(key),
                        label: host_label(host),
                        // The peer's own answer. There is no row here to derive
                        // it from, and a host can be up and simply hold no
                        // sessions.
                        unreachable: host.unreachable,
                        span: 0..0,
                        hidden: 0,
                        unfolded: unfolded.holds(Some(key)),
                    })
                }),
        );
        // Alphabetical by the label the header draws, byte order and no
        // casefolding: a section that holds still is worth more when the place
        // it holds still in can be guessed. `None` sorts first, which is the
        // rule it always had: a headerless run under someone else's header
        // would read as theirs. Hosts with no rows interleave here rather than
        // sinking to the bottom, because a host is looked up by its name
        // whether or not it holds anything. Two hosts reporting one name break
        // the tie by key, so a pair of clones of one repo holds an order at
        // all, and it is the order their ids give.
        groups.sort_by(|left, right| {
            left.label
                .cmp(&right.label)
                .then_with(|| left.key.cmp(&right.key))
        });
        let mut order = Vec::with_capacity(rows.len());
        for group in &mut groups {
            let start = order.len();
            // Taken rather than copied, so two groups drawing the same label
            // cannot both claim one host's rows.
            let members = gathered
                .iter_mut()
                .find(|(key, _)| *key == group.key)
                .map(|(_, members)| std::mem::take(members))
                .unwrap_or_default();
            if members.is_empty() {
                // A host the peer holds no rows for keeps the empty span at
                // its own place in the order, and the mark the peer gave it.
                group.span = start..start;
                continue;
            }
            group.unreachable = members
                .iter()
                .all(|&at| rows[at].status == RowStatus::Unreachable);
            let held = if group.unfolded {
                Vec::new()
            } else {
                held_back(rows, &members)
            };
            group.hidden = held.len();
            order.extend(members.iter().copied().filter(|at| !held.contains(at)));
            group.span = start..order.len();
        }
        let headed = groups.len() > 1;
        Self {
            rows,
            order,
            groups,
            headed,
        }
    }

    /// How many headers the hosts the peer holds no rows for need (spec 7.1).
    ///
    /// They are their headers and nothing else, so they take no part in the
    /// run: a run reaches into a span of rows, and these have none. That is
    /// also why [`Self::headed`] does not gate them, and why they are given a
    /// budget of their own. Suppressing the header of a group with no rows
    /// draws the host as nothing at all, which is exactly the absence the peer
    /// sends these entries to make visible.
    fn empty_headers(&self) -> usize {
        self.groups
            .iter()
            .filter(|group| group.span.is_empty() && group.label.is_some())
            .count()
    }

    /// How a strip `height` lines tall divides between the hosts the peer holds
    /// no rows for and the rows.
    ///
    /// The create row takes its line first, then those hosts. A row that loses
    /// its line still leaves the overflow count behind, while a host that lost
    /// its would leave nothing at all, so the hosts are the ones that cannot be
    /// cut while anything else can (spec 7.1).
    ///
    /// That holds only while the count itself has a line, so the rows keep one
    /// wherever they have any rows to count. Cutting the count too would leave
    /// the rows exactly as silent as the host would have been, which is the
    /// thing this ordering is for.
    fn split(&self, height: u16) -> Budget {
        let budget = usize::from(height).saturating_sub(1);
        let spare = budget.saturating_sub(usize::from(!self.order.is_empty()));
        let empty = self.empty_headers().min(spare);
        Budget {
            empty,
            rows: budget - empty,
        }
    }

    /// The header a run reaching into `group` draws for it, if any.
    fn header_of(&self, group: &Group<'a>, run: &Range<usize>) -> Option<&'a str> {
        if !self.headed {
            return None;
        }
        let host = group.label?;
        self.reaches(group, run).then_some(host)
    }

    /// Whether a run of the display order draws any of `group`'s rows.
    ///
    /// A group with no rows is reached by nothing, however the run falls
    /// around it. Its span is the empty range where it sits, which a run
    /// spanning that point would otherwise overlap, and it draws out of the
    /// budget of its own instead (see [`Self::split`]). Counting it here as
    /// well would charge its header twice and take the second line from the
    /// rows.
    fn reaches(&self, group: &Group<'a>, run: &Range<usize>) -> bool {
        !group.span.is_empty() && group.span.start < run.end && run.start < group.span.end
    }

    /// Whether `group` draws its fold line under a run that reaches it: it has
    /// rows the cap holds back, or the user unfolded it and the line is what
    /// folds it again (spec 9.2, the pointer must be able to undo what the
    /// pointer did).
    ///
    /// Tied to the run the same way a header is, so a group that draws a row
    /// draws its affordance: a cap that held rows back silently would be a
    /// strip that lies about what the peer offers.
    fn folds(&self, group: &Group<'a>, run: &Range<usize>) -> bool {
        (group.hidden > 0 || group.unfolded) && self.reaches(group, run)
    }

    /// Lines the run `order[run]` occupies: one per row, one per header and
    /// fold line it reaches, and one for the overflow row when it leaves any
    /// row out. The create row is not counted, it is paid for before a run is
    /// chosen.
    fn cost(&self, run: Range<usize>) -> usize {
        let chrome: usize = self
            .groups
            .iter()
            .map(|group| {
                usize::from(self.header_of(group, &run).is_some())
                    + usize::from(self.folds(group, &run))
            })
            .sum();
        chrome + run.len() + usize::from(run.len() < self.order.len())
    }

    /// The run of the display order to draw in `budget` lines: the longest one
    /// that fits, anchored where `scroll` says or around the focused row when
    /// it says nothing.
    ///
    /// Following focus scrolls by the least it can. The run stays anchored at
    /// the top until focus would fall past its bottom edge, which keeps a step
    /// from jumping the whole strip. With no focused row it shows the top of
    /// the order.
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

    /// The lines for a run, in draw order, within `budget`.
    fn lines(&self, run: Range<usize>, budget: Budget) -> Vec<StripLine> {
        let mut lines = Vec::with_capacity(run.len() + 2 * self.groups.len() + 2);
        let mut empty = 0;
        for group in &self.groups {
            if group.span.is_empty() {
                // A host the peer holds no rows for is its header and nothing
                // else, drawn where its label sorts rather than pushed to an
                // end (spec 7.1). It is drawn out of its own budget, so the
                // rows cannot crowd it out (see [`Self::split`]).
                if let Some(label) = group.label.filter(|_| empty < budget.empty) {
                    lines.push(StripLine::Header {
                        label: label.to_string(),
                        unreachable: group.unreachable,
                    });
                    empty += 1;
                }
                continue;
            }
            let from = group.span.start.max(run.start);
            let to = group.span.end.min(run.end);
            if from >= to {
                continue;
            }
            if let Some(label) = self.header_of(group, &run) {
                lines.push(StripLine::Header {
                    label: label.to_string(),
                    unreachable: group.unreachable,
                });
            }
            lines.extend((from..to).map(|at| StripLine::Session {
                index: self.order[at],
            }));
            if self.folds(group, &run) {
                lines.push(StripLine::Fold {
                    host: group.key.map(str::to_string),
                    hidden: group.hidden,
                });
            }
        }
        let hidden = self.order.len() - run.len();
        // At a height of one line the create row has the only line there is, so
        // even the count goes. [`Self::split`] keeps a line for it at every
        // other height where there is anything to count.
        if hidden > 0 && budget.rows > 0 {
            lines.push(StripLine::Overflow { hidden });
        }
        // The create row is always the last line, whatever else had to be left
        // out, because it is the affordance a pointer aims at.
        lines.push(StripLine::New);
        lines
    }
}

/// A host's `field` where it names something, `None` where it is absent or
/// empty.
///
/// An empty string is no name. A peer's word for a host arrives unpoliced (spec
/// 6.2), and nothing downstream can carry one: a strip group keyed on it says
/// nothing about the host it claims is there, and a create-flow row keyed on it
/// collides with the sentinel row that deliberately names none.
pub(crate) fn named(field: &Option<String>) -> Option<&str> {
    field.as_deref().filter(|text| !text.is_empty())
}

/// What a client calls one of a peer's hosts: the name that host reports for
/// itself, else the id its sessions are namespaced under, else the address the
/// peer has only ever known it by (spec 7.1).
///
/// One rule for every surface that names a host, so the strip's header and the
/// create-flow picker cannot label one host two ways. `None` for an entry
/// carrying none of the three, which is a shape no peer sends.
///
/// A label and never an id: a client that shows a name still groups rows and
/// addresses sessions by [`DirectoryHost::id`], and two hosts may report one
/// name (two clones of one repo) the way two sessions may share a tag.
pub(crate) fn host_label(host: &DirectoryHost) -> Option<&str> {
    named(&host.name)
        .or_else(|| named(&host.id))
        .or_else(|| named(&host.address))
}

/// What a session row reads its display label from: `id` without the exact
/// qualifier named by `host`, or the complete opaque id when that prefix is
/// absent or would leave no session component.
///
/// The host field is the authority for the qualifier. This function only
/// matches that supplied value and never discovers structure by parsing the
/// id, preserving the client-side opacity rule from spec 6.2.
pub(crate) fn session_label_source<'a>(id: &'a str, host: Option<&str>) -> &'a str {
    host.and_then(|host| id.strip_prefix(host)?.strip_prefix(':'))
        .filter(|session| !session.is_empty())
        .unwrap_or(id)
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
    tail_within_cols(&label, cols).to_string()
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

/// The longest tail of `text` that fits `cols` display columns, which is what
/// a label whose distinguishing part is its end keeps.
fn tail_within_cols(text: &str, cols: usize) -> &str {
    let Ok(budget) = u16::try_from(cols) else {
        return text;
    };
    if gwidth(text, Method::Unicode) <= budget {
        return text;
    }
    let mut kept = text.len();
    for (at, _) in text.char_indices().rev() {
        if gwidth(&text[at..], Method::Unicode) > budget {
            break;
        }
        kept = at;
    }
    &text[kept..]
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

/// `name` in `cols` display columns, cut at whichever end its shape says is
/// expendable and marked with an ellipsis.
///
/// A name holding a path separator is read as a path, and a path is told from
/// another by its tail, so the head goes: `…/work/umber/aj`. A name without one
/// reads left to right and its head is what its author chose first, so the tail
/// goes: `builder-1-ext…`. The rule keys on the shape rather than on how the
/// name was produced, because the wire deliberately does not say which it was
/// and should not grow a bit for typography (spec 6.1).
///
/// For a field of a known width, which is the strip's: the create-flow picker
/// hands its rows over whole, because a row is built before its overlay has a
/// width (see [`crate::host_picker`]).
fn elide_host_name(name: &str, cols: usize) -> String {
    if !name.contains('/') {
        return elide_to_cols(name, cols);
    }
    if width_of(name) <= cols {
        return name.to_string();
    }
    if cols == 0 {
        return String::new();
    }
    // The ellipsis has to fit beside whatever is kept, as it does the other way
    // round.
    format!("…{}", tail_within_cols(name, cols - 1))
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

/// A host header's label field: `label`, then a rule out to the field's edge,
/// with the unreachable mark set into the rule's tail.
///
/// The mark rides inside the rule rather than hanging off its end because the
/// rule has to reach the strip's edge either way.
fn header_field(label: &str, unreachable: bool, cols: usize) -> String {
    let mark = if unreachable { UNREACHABLE_MARK } else { "" };
    let mark_cols = mark.chars().count();
    // The label, one space, one rule character, and the mark, in that order.
    let name = elide_host_name(&one_line(label), cols.saturating_sub(mark_cols + 2));
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
    /// And a gesture names what the chord's handler works out for itself: a
    /// session it stepped to, or the group the focused row sits in. The two
    /// meet at the state the switch or the fold is applied to, not at the
    /// keymap, which is why the click needs no binding of its own.
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

    /// The style a label is drawn in: the working-set axis, as brightness, and
    /// the put-away axis, as a strike through the field.
    ///
    /// Two encodings because they answer two questions, and a revealed
    /// archived row has to answer both: an attached one is still attached, and
    /// dimming it to say "archived" would take back what brightness just said.
    /// A strike is what the app already draws through work that is done with
    /// (the todo list uses it for the same meaning), and it survives an
    /// archived row being the focused one, where brightness is spoken for.
    fn label_style(&self, row: &SidebarRow) -> Style {
        let presence = match row.presence() {
            Presence::Focused => self.styles.accent,
            Presence::Background => self.styles.text,
            Presence::Listed => self.styles.dim,
        };
        Style {
            strikethrough: row.archived,
            ..presence
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
            StripLine::Header { label, unreachable } => (
                " ",
                "~",
                dim,
                field(&header_field(label, *unreachable, cols), cols),
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
            StripLine::Fold { hidden, .. } => (
                " ",
                // The triangle is what tells this line from the overflow
                // count above the create row: that one says the height cut
                // rows off, this one says the group is holding them, and it
                // points at what it holds. A fold line is always its group's
                // last line, so what it holds is never below it: up at the
                // rows while they are on screen, right at the count that
                // stands in for them while they are not.
                if *hidden > 0 {
                    FOLDED_MARKER
                } else {
                    UNFOLDED_MARKER
                },
                dim,
                field(&fold_label(*hidden), cols),
                dim,
            ),
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

/// What a fold line says: how many rows its group is holding back, or that it
/// is holding none and this line is what folds it again.
fn fold_label(hidden: usize) -> String {
    if hidden > 0 {
        format!("{hidden} more")
    } else {
        "fold".to_string()
    }
}

/// What a click on `line` asks for, resolved against the rows the layout that
/// produced it was built from.
fn gesture_for(line: &StripLine, rows: &[SidebarRow]) -> Option<StripGesture> {
    match line {
        StripLine::Session { index } => Some(StripGesture::Focus(rows.get(*index)?.id.clone())),
        StripLine::Fold { host, .. } => Some(StripGesture::Fold(host.clone())),
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
        let cols = self.state.borrow().cols();
        let width = ctx.max.width.map_or(cols, |max| max.min(cols));
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
            archived: false,
            locked: false,
            lock_generation: None,
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

    /// A summary for a session the user has put away.
    fn archived(id: &str, minutes: i64) -> SessionSummary {
        SessionSummary {
            archived: true,
            ..at(id, minutes)
        }
    }

    /// The default view drops the rows the user archived, and the toggle puts
    /// them back where their ids say they go rather than in a group of their
    /// own: a revealed row is in the list the user is reading, and a second
    /// list would make them find it twice.
    #[test]
    fn archived_rows_leave_the_default_view_and_the_reveal_puts_them_back() {
        let rows = vec![
            at("session-a", 30),
            archived("session-b", 10),
            at("session-c", 20),
        ];
        let hidden = rows_for_display(&rows, "session-a", |_| false, |_| false, false);
        assert_eq!(
            ids(&hidden),
            vec!["session-c", "session-a"],
            "an archived row is in the default view",
        );
        let revealed = rows_for_display(&rows, "session-a", |_| false, |_| false, true);
        assert_eq!(
            ids(&revealed),
            vec!["session-c", "session-b", "session-a"],
            "the reveal moved a row out of its place",
        );
        assert!(
            revealed[1].archived,
            "the revealed row does not say it is archived, so nothing can draw it as such",
        );
    }

    /// The two exemptions: the session on screen and the ones the client holds
    /// open stay in the default view however archived they are, because the
    /// strip is what says where the working set is (spec 9.2). Archiving the
    /// focused session is allowed and leaves it on screen, so this is the
    /// state right after that gesture.
    #[test]
    fn an_archived_row_the_client_holds_open_stays_in_view() {
        let rows = vec![
            archived("session-a", 30),
            archived("session-b", 20),
            archived("session-c", 10),
        ];
        let display =
            rows_for_display(&rows, "session-c", |_| false, |id| id == "session-b", false);
        assert_eq!(
            ids(&display),
            vec!["session-c", "session-b"],
            "the working set is not what the archive bit hides",
        );
        assert_eq!(display[0].presence(), Presence::Focused);
        assert_eq!(display[1].presence(), Presence::Background);
    }

    /// A working turn does not exempt an archived row, which is what tells this
    /// filter from the cap. Archiving is the user saying they are done with the
    /// session, and the ruling allows it while the session works: the turn runs
    /// to its end out of sight.
    #[test]
    fn a_working_archived_row_still_leaves_the_view() {
        let working = SessionSummary {
            working: true,
            ..archived("session-b", 1)
        };
        let display = rows_for_display(
            &[at("session-a", 30), working],
            "session-a",
            |_| false,
            |_| false,
            false,
        );
        assert_eq!(
            ids(&display),
            vec!["session-a"],
            "a turn inside an archived session put its row back",
        );
    }

    /// Archived rows are gone before the cap counts, so a group's five places
    /// go to rows the user can see. A filter that ran after the cap would spend
    /// places on rows it then dropped, and the group would draw short.
    #[test]
    fn an_archived_row_spends_none_of_a_groups_cap() {
        let mut rows: Vec<SessionSummary> = (0..GROUP_CAP)
            .map(|at| archived(&format!("session-a-{at}"), 1))
            .collect();
        rows.extend((0..GROUP_CAP).map(|at| self::at(&format!("session-b-{at}"), 2)));
        let display = rows_for_display(&rows, "none", |_| false, |_| false, false);
        let lines = folded(&display, &[], 20);
        assert!(
            folds(&lines).is_empty(),
            "the cap held a row back over sessions the strip is not showing: {lines:?}",
        );
        assert_eq!(
            display.len(),
            GROUP_CAP,
            "the archived rows are in the set, so the group was never full",
        );
    }

    /// Rows sit where their ids put them, newest minted id first, whatever the
    /// activity on them says. A row that climbed to the top when a message
    /// arrived would move out from under a pointer aimed at it, which is the
    /// whole reason the strip holds still (spec 9.2).
    #[test]
    fn rows_order_by_id_not_by_activity() {
        // Activity descends as the ids ascend, so an activity-ordered result
        // is the exact reverse and cannot pass by accident.
        let rows = vec![
            at("session-a", 30),
            at("session-b", 10),
            at("session-c", 20),
        ];
        let display = rows_for_display(&rows, "session-b", |_| false, |_| false, false);
        assert_eq!(ids(&display), vec!["session-c", "session-b", "session-a"]);
        assert!(display[1].focused, "the focused row is marked");
        assert!(!display[0].focused);

        // The stamps move, the rows do not. The busiest session is now the
        // oldest id, and it stays at the bottom where its id puts it.
        let stirred = vec![at("session-a", 0), at("session-b", 40), at("session-c", 40)];
        let display = rows_for_display(&stirred, "session-b", |_| false, |_| false, false);
        assert_eq!(
            ids(&display),
            vec!["session-c", "session-b", "session-a"],
            "activity moved and the order did not",
        );
    }

    /// The row carries the stamp the cap selects on, so the layout can choose
    /// which rows survive without the order having to encode it.
    #[test]
    fn a_row_carries_the_activity_the_cap_reads() {
        let rows = vec![at("session-a", 30), at("session-b", 10)];
        let display = rows_for_display(&rows, "session-a", |_| false, |_| false, false);
        assert_eq!(
            display[0].last_activity, rows[1].last_activity,
            "session-b's stamp rode into its row",
        );
        assert!(
            display[1].last_activity < display[0].last_activity,
            "and the older row kept the older stamp",
        );
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
            false,
        );
        assert_eq!(display[1].tag.as_deref(), Some("fix-auth"));
        assert_eq!(display[1].host.as_deref(), Some("builder-1"));
        assert_eq!(display[1].presence(), Presence::Background);
        assert_eq!(
            display[0].presence(),
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
                archived: false,
                last_activity: DateTime::UNIX_EPOCH,
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

        fn archived(mut self) -> Self {
            self.row.archived = true;
            self
        }

        /// How long ago the row last did something, which is what the cap
        /// selects on.
        fn active(mut self, minutes_ago: i64) -> Self {
            self.row.last_activity =
                DateTime::UNIX_EPOCH + chrono::Duration::minutes(1_000 - minutes_ago);
            self
        }

        fn build(self) -> SidebarRow {
            self.row
        }
    }

    fn rows_named(ids: &[&str]) -> Vec<SidebarRow> {
        ids.iter().map(|id| row(id).build()).collect()
    }

    /// The strip's lines with every group folded and no wheel anchor, which is
    /// the resting state.
    fn folded(rows: &[SidebarRow], hosts: &[DirectoryHost], height: u16) -> Vec<StripLine> {
        strip_lines(rows, hosts, &Unfolded::default(), height, None)
    }

    /// The strip's lines with `group` unfolded.
    fn unfolded(
        rows: &[SidebarRow],
        hosts: &[DirectoryHost],
        group: Option<&str>,
        height: u16,
    ) -> Vec<StripLine> {
        let mut open = Unfolded::default();
        open.toggle(group);
        strip_lines(rows, hosts, &open, height, None)
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

    /// A plain host holding eight rows, of which the cap holds two back.
    ///
    /// The two it holds (`s-7` and `s-6`) sit in the middle of the group, so
    /// stepping over them in either direction lands somewhere a walk of the
    /// mirror's own rows would not.
    fn stepping_state(focused: &str) -> SidebarState {
        let rows = [
            ("s-8", 6),
            ("s-7", 90),
            ("s-6", 91),
            ("s-5", 5),
            ("s-4", 4),
            ("s-3", 3),
            ("s-2", 2),
            ("s-1", 1),
        ]
        .into_iter()
        .map(|(id, minutes)| {
            let built = row(id).active(minutes);
            if id == focused {
                built.focused()
            } else {
                built
            }
            .build()
        })
        .collect();
        SidebarState {
            visible: true,
            rows,
            ..SidebarState::default()
        }
    }

    /// Stepping walks what the strip draws, so it steps over the rows the cap
    /// holds back rather than focusing them one at a time. Each of those
    /// focuses would attach a session the user cannot even see (spec 9.2).
    #[test]
    fn stepping_steps_over_the_rows_the_cap_holds_back() {
        let state = stepping_state("s-8");
        assert_eq!(
            drawn(&state.lines(20), &state.rows),
            vec!["s-8", "s-5", "s-4", "s-3", "s-2", "s-1"],
            "the fixture folds two rows away, or this test measures nothing",
        );
        assert_eq!(
            step_session(&state, true).as_deref(),
            Some("s-5"),
            "forward past the two folded rows",
        );
        assert_eq!(
            step_session(&state, false).as_deref(),
            Some("s-1"),
            "and backward off the top wraps to the last drawn row",
        );

        let state = stepping_state("s-5");
        assert_eq!(
            step_session(&state, false).as_deref(),
            Some("s-8"),
            "backward past them too",
        );
        assert_eq!(step_session(&state, true).as_deref(), Some("s-4"));
    }

    /// Unfolding a group puts its rows back in the walk, because stepping and
    /// the strip read the same layout rather than two copies of the rule.
    #[test]
    fn stepping_walks_a_group_the_user_unfolded() {
        let mut state = stepping_state("s-8");
        state.toggle_fold(None);
        assert_eq!(step_session(&state, true).as_deref(), Some("s-7"));
        state.toggle_fold(None);
        assert_eq!(
            step_session(&state, true).as_deref(),
            Some("s-5"),
            "and folding it again takes them back out",
        );
    }

    /// Stepping walks the strip's grouped order, not the order the rows sit in
    /// the mirror. Those differ as soon as a peer's rows interleave two hosts,
    /// and the chord has to move the way the eye does.
    #[test]
    fn stepping_walks_the_order_the_strip_draws() {
        let state = SidebarState {
            visible: true,
            rows: vec![
                row("s-3").host("laptop").focused().build(),
                row("s-2").host("builder-1").build(),
                row("s-1").host("laptop").build(),
            ],
            ..SidebarState::default()
        };
        assert_eq!(
            drawn(&state.lines(20), &state.rows),
            vec!["s-2", "s-3", "s-1"],
            "the strip groups them, and the fixture interleaves the hosts",
        );
        assert_eq!(
            step_session(&state, true).as_deref(),
            Some("s-1"),
            "forward is the row below the focused one on screen",
        );
        assert_eq!(
            step_session(&state, false).as_deref(),
            Some("s-2"),
            "and backward the row above it",
        );
    }

    /// The fold is the user's, and no row carries it, so replacing every row
    /// leaves the group open. That the drive loop's own refresh does the same
    /// is pinned where the refresh runs, in the shell's sidebar tests.
    #[test]
    fn an_unfolded_group_outlives_the_rows_it_was_made_on() {
        let mut state = stepping_state("s-8");
        state.toggle_fold(None);
        assert_eq!(drawn(&state.lines(20), &state.rows).len(), 8);

        state.rows = stepping_state("s-8").rows;
        assert_eq!(
            drawn(&state.lines(20), &state.rows).len(),
            8,
            "the group is still open after the rows were replaced",
        );
    }

    /// The chord folds the group the focused row sits in, and nothing else. It
    /// reads the group off that row because that is the one the user is
    /// looking at, and it says whether there was a row to read.
    #[test]
    fn the_chord_folds_the_group_the_focused_row_sits_in() {
        let mut rows: Vec<SidebarRow> = (1..=8)
            .map(|n| row(&format!("b-{n}")).host("builder-1").active(n).build())
            .collect();
        rows.extend((1..=8).map(|n| row(&format!("l-{n}")).host("laptop").active(n).build()));
        rows[8].focused = true;
        let mut state = SidebarState {
            visible: true,
            rows,
            ..SidebarState::default()
        };
        assert!(state.toggle_focused_fold(), "there was a focused row");
        assert_eq!(
            folds(&state.lines(30)),
            vec![(Some("builder-1"), 3), (Some("laptop"), 0)],
            "the focused row's host opened and the other did not",
        );

        assert!(state.toggle_focused_fold());
        assert_eq!(
            folds(&state.lines(30)),
            vec![(Some("builder-1"), 3), (Some("laptop"), 2)],
            "and the same chord folds it back, the focused row being exempt",
        );

        state.rows[8].focused = false;
        assert!(
            !state.toggle_focused_fold(),
            "with no focused row there is no group to work on",
        );
        assert_eq!(
            folds(&state.lines(30)),
            vec![(Some("builder-1"), 3), (Some("laptop"), 3)],
            "and nothing moved, beyond the row that stopped being exempt",
        );
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
                StripLine::Header { label, unreachable } => Some((label.as_str(), *unreachable)),
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

    /// The fold lines a layout draws, as the group they belong to and the
    /// count they hold back, in order.
    fn folds(lines: &[StripLine]) -> Vec<(Option<&str>, usize)> {
        lines
            .iter()
            .filter_map(|line| match line {
                StripLine::Fold { host, hidden } => Some((host.as_deref(), *hidden)),
                _ => None,
            })
            .collect()
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
            let lines = folded(&rows, &[], height);
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
        assert!(folded(&rows_named(&["a", "b"]), &[], 0).is_empty());
    }

    /// One host, or none at all, is not a grouping. A plain connect has to look
    /// exactly as it would have before hosts existed (spec 9.2).
    #[test]
    fn a_single_host_gets_no_headers() {
        let hostless = rows_named(&["a", "b", "c"]);
        assert!(
            headers(&folded(&hostless, &[], 20)).is_empty(),
            "rows with no host name nothing to group under",
        );
        let one_host: Vec<SidebarRow> = ["a", "b", "c"]
            .iter()
            .map(|id| row(id).host("builder-1").build())
            .collect();
        let lines = folded(&one_host, &[], 20);
        assert!(
            headers(&lines).is_empty(),
            "one host is not a grouping: {lines:?}",
        );
        assert_eq!(drawn(&lines, &one_host), vec!["a", "b", "c"]);
    }

    /// Distinct hosts group, the groups sit where their labels sort, and each
    /// group keeps the order its rows arrived in. Nothing here reads activity:
    /// a section that floated on it would move the click targets under the
    /// pointer (spec 9.2).
    #[test]
    fn groups_sit_where_their_labels_sort() {
        // The rows arrive as the display order has them, id descending (see
        // `rows_for_display`), which puts the hosts in the reverse of the
        // order they are drawn in: a layout that kept the order it was given
        // would differ.
        let rows = vec![
            row("laptop-b").host("laptop").active(0).build(),
            row("laptop-a").host("laptop").active(30).build(),
            row("builder-b").host("builder-1").active(1).build(),
            row("builder-a").host("builder-1").active(31).build(),
        ];
        let lines = folded(&rows, &[], 20);
        assert_eq!(
            headers(&lines),
            vec![("builder-1", false), ("laptop", false)],
            "alphabetical by the label the header draws, and the busiest host \
             is the one that sorts second",
        );
        assert_eq!(
            drawn(&lines, &rows),
            vec!["builder-b", "builder-a", "laptop-b", "laptop-a"],
            "and a group holds its rows in the order it was handed them",
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
            headers(&folded(&rows, &[], 20)),
            vec![("builder-1", false), ("laptop", true)],
        );
        rows[1].status = RowStatus::Idle;
        assert_eq!(
            headers(&folded(&rows, &[], 20)),
            vec![("builder-1", false), ("laptop", false)],
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
        let lines = folded(&rows, &[], 20);
        assert_eq!(drawn(&lines, &rows), vec!["nameless", "named"]);
        assert_eq!(headers(&lines), vec![("builder-1", false)]);
        assert!(
            matches!(lines[0], StripLine::Session { .. }),
            "the nameless row is above every header: {lines:?}",
        );
    }

    /// A host the peer holds rows for is grouped by those rows, and the peer
    /// naming it in the directory adds no second, empty group beside them.
    #[test]
    fn a_host_the_peer_holds_rows_for_groups_by_its_rows() {
        let rows = vec![
            row("laptop-b").host("laptop").build(),
            row("laptop-a").host("laptop").build(),
            row("builder-a").host("builder-1").build(),
        ];
        let hosts = vec![learned("laptop", false), learned("builder-1", false)];
        let lines = folded(&rows, &hosts, 20);
        assert_eq!(
            headers(&lines),
            vec![("builder-1", false), ("laptop", false)],
            "one group per host and no empty twin beside either: {lines:?}",
        );
        assert_eq!(
            drawn(&lines, &rows),
            vec!["builder-a", "laptop-b", "laptop-a"],
        );
    }

    /// A host the peer holds no rows for is drawn as an empty group: its
    /// header with nothing under it, sitting where its label sorts among the
    /// hosts that do have rows.
    ///
    /// This is the case the directory's host entries exist for. A gateway holds
    /// a host's rows only for as long as that host has sent them, so across a
    /// restart it has none for a host that is down, and a strip grouping by the
    /// rows alone would draw that host as nothing at all (spec 7.1). It
    /// interleaves rather than sinking to the bottom, because a host is looked
    /// up by its name whether or not it is holding anything.
    #[test]
    fn a_host_the_peer_holds_no_rows_for_draws_an_empty_group() {
        let rows = vec![row("s-1").host("builder-1").build()];
        let hosts = vec![learned("builder-1", false), learned("aleph", true)];
        let lines = folded(&rows, &hosts, 20);
        assert_eq!(
            headers(&lines),
            vec![("aleph", true), ("builder-1", false)],
            "the host with no rows is a group of its own: {lines:?}",
        );
        assert_eq!(
            lines,
            vec![
                StripLine::Header {
                    label: "aleph".to_string(),
                    unreachable: true,
                },
                StripLine::Header {
                    label: "builder-1".to_string(),
                    unreachable: false,
                },
                StripLine::Session { index: 0 },
                StripLine::New,
            ],
            "and it sits above the host it sorts above, with no rows under it",
        );
    }

    /// A host the gateway has never reached has no id to be named by, so its
    /// group is labelled by the address it is configured at. The gateway must
    /// not invent an id for it, and the strip has nothing else to show.
    #[test]
    fn a_host_with_no_id_is_labelled_by_its_address() {
        let rows = vec![row("s-1").host("builder-1").build()];
        let hosts = vec![learned("builder-1", false), configured("10.0.0.7:7777")];
        let lines = folded(&rows, &hosts, 20);
        assert_eq!(
            headers(&lines),
            vec![("10.0.0.7:7777", true), ("builder-1", false)],
            "{lines:?}",
        );
    }

    /// Several hosts can be waiting on their first contact at once, so an
    /// address labels more than one group. It is a label and never an id, so
    /// nothing keys a group by it.
    #[test]
    fn two_hosts_can_both_be_labelled_by_their_addresses() {
        // Named the other way round by the peer, so what places them here is
        // the label rather than the order they arrived in.
        let hosts = vec![configured("10.0.0.8:7777"), configured("10.0.0.7:7777")];
        let lines = folded(&[], &hosts, 20);
        assert_eq!(
            headers(&lines),
            vec![("10.0.0.7:7777", true), ("10.0.0.8:7777", true)],
            "both hosts are drawn, where their labels sort: {lines:?}",
        );
    }

    /// Labels sort in byte order with no casefolding, and two hosts drawing one
    /// label under one key keep the order the peer named them in.
    ///
    /// Byte order is the predictable rule: it needs no table and no locale, and
    /// the cost is that a capitalised name sorts above every lowercase one. The
    /// tie-break matters because a host known only by an address can draw the
    /// same label as another host's id, and two groups swapping places between
    /// frames is the thing this order exists to prevent.
    #[test]
    fn labels_sort_in_byte_order_and_ties_keep_the_peers_order() {
        let hosts = vec![learned("aaa", false), learned("Zeta", false)];
        assert_eq!(
            headers(&folded(&[], &hosts, 20)),
            vec![("Zeta", false), ("aaa", false)],
            "casefolding would have put aaa first",
        );

        // Both draw "dup", one an id the peer can reach and one an address it
        // cannot, so the marks tell the order apart.
        let twins = vec![learned("dup", false), configured("dup")];
        assert_eq!(
            headers(&folded(&[], &twins, 20)),
            vec![("dup", false), ("dup", true)],
            "the tie kept the order the peer named them in",
        );
    }

    /// A group reads as the name its host reports for itself, whether the group
    /// holds rows or is a host the peer holds none for. The id is what the rows
    /// carry and what a session is addressed by, so it stays the fallback and
    /// never the label of a host that named itself (spec 7.1).
    #[test]
    fn a_group_reads_as_the_name_its_host_reports() {
        let rows = vec![row("s-1").host("290dc828").build()];
        let hosts = vec![
            calling_itself("290dc828", "~/work/umber/aj", false),
            calling_itself("cbcfaabe", "~/workshop", true),
            learned("nameless", false),
            configured("10.0.0.7:7777"),
        ];
        let lines = folded(&rows, &hosts, 20);
        assert_eq!(
            headers(&lines),
            vec![
                ("10.0.0.7:7777", true),
                ("nameless", false),
                ("~/work/umber/aj", false),
                ("~/workshop", true),
            ],
            "a named host reads as its name, one that reported none as its id, \
             and one the peer has never spoken to as its address: {lines:?}",
        );
        assert_eq!(
            drawn(&lines, &rows),
            vec!["s-1"],
            "and the row still sits under its own host, which it names by id",
        );
    }

    /// Groups sit where their *labels* sort, so the strip reorders the moment
    /// hosts carry names: the id order and the name order need not agree, and
    /// the order a reader can predict is the one they can see.
    ///
    /// Two hosts reporting one name break the tie by the id their rows carry, so
    /// a pair of clones of one repo holds an order rather than swapping places
    /// between frames.
    #[test]
    fn groups_sort_by_name_once_their_hosts_are_named() {
        let hosts = vec![
            calling_itself("aaa", "~/workshop", false),
            calling_itself("bbb", "~/work/umber/aj", false),
        ];
        assert_eq!(
            headers(&folded(&[], &hosts, 20)),
            vec![("~/work/umber/aj", false), ("~/workshop", false)],
            "the id order is the opposite of the name order, and the names win",
        );

        let twins = vec![
            calling_itself("bbb", "~/work/aj", false),
            calling_itself("aaa", "~/work/aj", true),
        ];
        assert_eq!(
            headers(&folded(&[], &twins, 20)),
            vec![("~/work/aj", true), ("~/work/aj", false)],
            "one label, and the ids place them: the unreachable one is aaa",
        );
    }

    /// A fold is remembered by the id a group's rows carry, not by what its
    /// header reads. The pointer's fold line and the chord's focused row have to
    /// name the same group, and only the id is the same string on both paths.
    #[test]
    fn a_fold_is_keyed_on_the_host_id_and_not_on_its_label() {
        let mut state = SidebarState {
            visible: true,
            rows: (0..8)
                .map(|at| row(&format!("s-{at}")).host("290dc828").build())
                .collect(),
            hosts: vec![
                calling_itself("290dc828", "~/work/umber/aj", false),
                learned("other", false),
            ],
            ..SidebarState::default()
        };
        let lines = state.lines(30);
        assert_eq!(
            headers(&lines),
            vec![("other", false), ("~/work/umber/aj", false)],
            "the header of the group holding the rows reads the name: {lines:?}",
        );
        assert_eq!(
            folds(&lines),
            vec![(Some("290dc828"), 3)],
            "and the fold line names the host by id, which is what a click on it \
             hands back: {lines:?}",
        );

        state.toggle_fold(Some("290dc828"));
        assert_eq!(
            folds(&state.lines(30)),
            vec![(Some("290dc828"), 0)],
            "so the pointer and the chord fold one group and not two",
        );
    }

    /// A host named both ways at once is labelled by the id. The id is what its
    /// sessions are namespaced under, so it is the name the rest of the strip is
    /// read against, and the address only ever stands in for it while there is
    /// none (spec 7.1).
    #[test]
    fn a_host_named_both_ways_is_labelled_by_its_id() {
        let both = DirectoryHost {
            id: Some("builder-2".to_string()),
            address: Some("10.0.0.8:7777".to_string()),
            name: None,
            working_directory: None,
            unreachable: true,
        };
        let lines = folded(&[], &[learned("builder-1", false), both], 20);
        assert_eq!(
            headers(&lines),
            vec![("builder-1", false), ("builder-2", true)],
            "{lines:?}",
        );
    }

    /// An address is a label and never an id, so it cannot stand in for one when
    /// a host is matched to the rows it owns. A host named by an address keeps
    /// its own empty group even where that address reads exactly like the id
    /// another host's rows carry, because the two are different hosts and
    /// merging them would drop one the peer named.
    #[test]
    fn an_address_never_matches_a_group_named_by_an_id() {
        let rows = vec![row("s-1").host("builder-1").build()];
        let hosts = vec![learned("builder-1", false), configured("builder-1")];
        let lines = folded(&rows, &hosts, 20);
        assert_eq!(
            headers(&lines),
            vec![("builder-1", false), ("builder-1", true)],
            "two hosts, and only one of them holds rows: {lines:?}",
        );
        assert_eq!(drawn(&lines, &rows), vec!["s-1"]);
    }

    /// A host's first contact relabels its group from the address to the id it
    /// answered with, and leaves it one group: the id is news about the host
    /// the strip was already drawing, not a second host.
    #[test]
    fn a_hosts_first_contact_relabels_its_empty_group() {
        let rows = vec![row("s-1").host("builder-1").build()];
        let waiting = vec![learned("builder-1", false), configured("10.0.0.8:7777")];
        assert_eq!(
            headers(&folded(&rows, &waiting, 20)),
            vec![("10.0.0.8:7777", true), ("builder-1", false)],
        );

        // It answers, so the gateway has its id and can reach it. Its rows are
        // not here yet: those arrive on its own `list` frame.
        let met = vec![learned("builder-1", false), learned("builder-2", false)];
        let lines = folded(&rows, &met, 20);
        assert_eq!(
            headers(&lines),
            vec![("builder-1", false), ("builder-2", false)],
            "the group is relabelled, and it is still one group: {lines:?}",
        );
    }

    /// One host is not a grouping, but a host with no rows is nothing except
    /// its header. It draws one where a single group otherwise would not,
    /// because the alternative is drawing the host as nothing at all.
    #[test]
    fn an_empty_group_draws_where_a_single_host_would_get_no_header() {
        let lines = folded(&[], &[learned("laptop", true)], 20);
        assert_eq!(
            headers(&lines),
            vec![("laptop", true)],
            "the only host there is, and it is unreachable: {lines:?}",
        );

        // And a lone host that does hold rows still reads as it did before
        // hosts existed.
        let rows = vec![row("s-1").host("builder-1").build()];
        let lines = folded(&rows, &[learned("builder-1", false)], 20);
        assert!(
            headers(&lines).is_empty(),
            "one host with rows is not a grouping: {lines:?}",
        );
    }

    /// The mark on a group with no rows is the peer's own answer about that
    /// host. There is nothing to derive it from: "the peer can reach none of
    /// these rows" is vacuously true of no rows at all, and would put the mark
    /// on a host that is up and simply holds no sessions.
    #[test]
    fn an_empty_groups_mark_is_the_peers_own_answer() {
        let hosts = vec![learned("up-and-empty", false), learned("gone", true)];
        let lines = folded(&[], &hosts, 20);
        assert_eq!(
            headers(&lines),
            vec![("gone", true), ("up-and-empty", false)],
            "{lines:?}",
        );
    }

    /// A host entry naming neither an id nor an address says nothing a header
    /// could show, so nothing is drawn for it. A blank header would claim a
    /// host is there and refuse to say which.
    ///
    /// An id or an address with nothing in it is that same entry: a client does
    /// not police the grammar a gateway enforces at enrollment, so this is the
    /// peer's word taken as it arrives (spec 6.2). Such an entry is not a group
    /// at all, which is also what keeps it from turning the one real host's
    /// single run into a grouping that wears a header.
    #[test]
    fn a_host_named_by_neither_an_id_nor_an_address_draws_nothing() {
        let nameless = DirectoryHost {
            id: None,
            address: None,
            name: None,
            working_directory: None,
            unreachable: true,
        };
        let blank = DirectoryHost {
            id: Some(String::new()),
            address: Some(String::new()),
            name: Some(String::new()),
            working_directory: None,
            unreachable: true,
        };
        for entry in [nameless, blank] {
            let hosts = vec![learned("builder-1", true), entry.clone()];
            let lines = folded(&[], &hosts, 20);
            assert_eq!(
                headers(&lines),
                vec![("builder-1", true)],
                "{entry:?} drew a header: {lines:?}",
            );
            assert_eq!(
                lines.len(),
                2,
                "one header and the create row, nothing else: {lines:?}",
            );
        }

        // The rows of one real host are one run, and an entry that names nothing
        // is not a second group to head them with.
        let rows = vec![row("s-1").host("builder-1").build()];
        let hosts = vec![
            learned("builder-1", false),
            DirectoryHost {
                id: Some(String::new()),
                address: None,
                name: None,
                working_directory: None,
                unreachable: true,
            },
        ];
        let lines = folded(&rows, &hosts, 20);
        assert!(
            headers(&lines).is_empty(),
            "one host with rows is still not a grouping: {lines:?}",
        );
    }

    /// A host the peer holds no rows for takes its line before the rows do. A
    /// row that loses its line leaves the overflow count behind, so cutting a
    /// row still tells the user something was cut, while cutting the host would
    /// leave nothing at all.
    #[test]
    fn an_empty_group_takes_its_line_before_the_rows() {
        let rows = rows_named(&["a", "b", "c"]);
        // Four lines hold all three rows and the create row with no host in
        // play, so the host is what displaces them.
        assert_eq!(drawn(&folded(&rows, &[], 4), &rows), vec!["a", "b", "c"],);

        let lines = folded(&rows, &[configured("10.0.0.7:7777")], 4);
        assert_eq!(
            headers(&lines),
            vec![("10.0.0.7:7777", true)],
            "the host kept its line: {lines:?}",
        );
        assert_eq!(drawn(&lines, &rows), vec!["a"], "the rows gave theirs up");
        assert_eq!(hidden(&lines), Some(2), "and they say so: {lines:?}");
        assert_eq!(lines.len(), 4, "inside the height: {lines:?}");
    }

    /// A host with no rows costs the strip exactly one line, wherever its
    /// label sorts it to.
    ///
    /// Its header is drawn out of a budget of its own, so a layout that also
    /// charged the run for it would take a second line from the rows and hide
    /// a row it had room for. That can only happen where the host sorts
    /// between two hosts that do have rows, which is why the fixture puts it
    /// there and why the same strip is measured with the host renamed to sort
    /// last.
    #[test]
    fn a_host_with_no_rows_costs_one_line_wherever_it_sorts() {
        let rows = vec![
            row("s-a").host("aaa").build(),
            row("s-c").host("ccc").build(),
        ];
        let between = vec![
            learned("aaa", false),
            learned("bbb", true),
            learned("ccc", false),
        ];
        let lines = folded(&rows, &between, 6);
        assert_eq!(
            drawn(&lines, &rows),
            vec!["s-a", "s-c"],
            "both rows fit beside the three headers and the create row: \
             {lines:?}",
        );
        assert_eq!(hidden(&lines), None, "so nothing was cut: {lines:?}");
        assert_eq!(lines.len(), 6, "and the height is spent: {lines:?}");

        let after = vec![
            learned("aaa", false),
            learned("ccc", false),
            learned("zzz", true),
        ];
        assert_eq!(
            folded(&rows, &after, 6).len(),
            lines.len(),
            "the same strip costs the same whether the rowless host sorts \
             between the other two or after them",
        );
    }

    /// A host the user unfolded that later holds no rows costs nothing either.
    /// It draws no fold line (there is nothing to fold), so charging the run
    /// for one would take a line for something the strip never draws, and no
    /// affordance would be left to get it back.
    #[test]
    fn a_rowless_host_the_user_unfolded_costs_no_fold_line() {
        let rows = vec![
            row("s-a").host("aaa").build(),
            row("s-c").host("ccc").build(),
        ];
        let hosts = vec![
            learned("aaa", false),
            learned("bbb", true),
            learned("ccc", false),
        ];
        let lines = unfolded(&rows, &hosts, Some("bbb"), 6);
        assert!(folds(&lines).is_empty(), "no fold line is drawn: {lines:?}");
        assert_eq!(
            drawn(&lines, &rows),
            vec!["s-a", "s-c"],
            "and no row lost its line to one: {lines:?}",
        );
    }

    /// A fold line is chrome the group pays for, so a run that reaches no row
    /// of a group pays for neither its header nor its fold line.
    #[test]
    fn a_group_the_run_does_not_reach_costs_nothing() {
        let mut rows: Vec<SidebarRow> = (1..=8)
            .map(|n| row(&format!("a-{n}")).host("aaa").active(n).build())
            .collect();
        rows.extend((1..=8).map(|n| row(&format!("z-{n}")).host("zzz").active(n).build()));
        // Six lines: the create row, the overflow count, and four for the
        // first group's header, two of its rows and its fold line. The second
        // group is out of the run and costs nothing at all.
        let lines = folded(&rows, &[], 6);
        assert_eq!(headers(&lines), vec![("aaa", false)], "{lines:?}");
        assert_eq!(folds(&lines), vec![(Some("aaa"), 3)], "{lines:?}");
        assert_eq!(drawn(&lines, &rows).len(), 2, "{lines:?}");
        assert_eq!(lines.len(), 6, "{lines:?}");
    }

    /// The mark on a header answers for the host, not for the rows that
    /// happened to survive the cap. A host with one reachable session is
    /// reachable even when the cap holds that session back, or the strip would
    /// declare a host out on the strength of what it is not showing.
    #[test]
    fn the_header_mark_reads_every_row_the_cap_hid() {
        let mut rows: Vec<SidebarRow> = (1..=5)
            .map(|n| {
                row(&format!("gone-{n}"))
                    .host("laptop")
                    .status(RowStatus::Unreachable)
                    .active(n)
                    .build()
            })
            .collect();
        // Older than all of them, so the cap is what takes it off screen.
        rows.push(row("here").host("laptop").active(90).build());
        let lines = folded(&rows, &[learned("zeta", false)], 20);
        assert_eq!(
            drawn(&lines, &rows).len(),
            5,
            "the reachable row is the one the cap held: {lines:?}",
        );
        assert_eq!(
            headers(&lines),
            vec![("laptop", false), ("zeta", false)],
            "and the host is still answering: {lines:?}",
        );
    }

    /// Folding drops the wheel's anchor, so the gesture always shows its own
    /// result. An anchor is an index into a display order the fold just
    /// changed: kept, it would leave the user looking at rows they did not ask
    /// for, and an unfold whose rows all land above it would look like a
    /// gesture that did nothing.
    #[test]
    fn folding_hands_the_strip_back_to_focus_following() {
        let mut state = SidebarState {
            visible: true,
            rows: (1..=12)
                .map(|n| row(&format!("s-{n:02}")).active(n).build())
                .collect(),
            ..SidebarState::default()
        };
        let mut moved = 0;
        for _ in 0..8 {
            if state.scroll_by(1, 6) {
                moved += 1;
            }
        }
        assert!(
            moved > 0,
            "the wheel never moved the run, so this test measures nothing",
        );
        let scrolled = drawn(&state.lines(6), &state.rows);
        assert!(
            !scrolled.contains(&"s-01"),
            "the wheel moved the run off the top, or this test measures \
             nothing: {scrolled:?}",
        );

        state.toggle_fold(None);
        let opened = drawn(&state.lines(6), &state.rows);
        assert_eq!(
            opened.first(),
            Some(&"s-01"),
            "unfolding drew the group from its top rather than leaving the run \
             where the wheel had it, where the rows it revealed would all sit \
             above the view: {opened:?}",
        );
    }

    /// Hosts with no rows can eat the whole budget, and the strip still stays
    /// inside its height with the create row on the last line.
    #[test]
    fn hosts_with_no_rows_stay_inside_the_height() {
        let rows = rows_named(&["a", "b"]);
        let hosts: Vec<DirectoryHost> = (0..4)
            .map(|n| configured(&format!("10.0.0.{n}:7777")))
            .collect();
        for height in 0..=8 {
            let lines = folded(&rows, &hosts, height);
            assert!(
                lines.len() <= usize::from(height),
                "{height} lines held {lines:?}",
            );
            if height > 0 {
                assert_eq!(
                    lines.last(),
                    Some(&StripLine::New),
                    "{height} lines put the create row last: {lines:?}",
                );
            }
        }

        // Three lines: the create row, the count of what did not fit, and one
        // host. The rows keep a line wherever they have anything to count, so
        // the hosts take what is left rather than all of it.
        let lines = folded(&rows, &hosts, 3);
        assert_eq!(hidden(&lines), Some(2), "the rows still say so: {lines:?}");
        assert_eq!(headers(&lines).len(), 1, "{lines:?}");

        // With no rows there is nothing to count, and the hosts have the whole
        // budget.
        let lines = folded(&[], &hosts, 3);
        assert_eq!(headers(&lines).len(), 2, "{lines:?}");
        assert_eq!(hidden(&lines), None, "{lines:?}");
    }

    /// The wheel and the draw divide the height the same way, so a strip with a
    /// host it holds no rows for can still be scrolled to its last row. A wheel
    /// measuring against a budget the draw does not use stops short, and the
    /// rows past where it stops become unreachable.
    #[test]
    fn the_wheel_reaches_the_last_row_past_an_empty_group() {
        let ids: Vec<String> = (0..10).map(|i| format!("s-{i}")).collect();
        let mut state = SidebarState {
            visible: true,
            // Attached, so every row is on screen to be scrolled to and the
            // height is the only thing cutting any of them (see [`GROUP_CAP`]).
            rows: ids.iter().map(|id| row(id).attached().build()).collect(),
            hosts: vec![configured("10.0.0.7:7777")],
            ..SidebarState::default()
        };
        let mut moved = 0;
        for _ in 0..20 {
            if state.scroll_by(1, 6) {
                moved += 1;
            }
        }
        assert!(
            moved > 0,
            "the wheel never moved the run, so the test measures nothing",
        );
        let lines = state.lines(6);
        assert!(
            drawn(&lines, &state.rows).contains(&"s-9"),
            "the wheel reached the last row: {lines:?}",
        );
    }

    /// Rows that do not fit are counted, not dropped in silence. What is cut is
    /// the tail of the display order, which is where the run ends.
    #[test]
    fn the_rows_that_do_not_fit_are_counted() {
        let rows = rows_named(&["a", "b", "c", "d", "e"]);
        // Six lines: five rows and the create row, so nothing is cut.
        let whole = folded(&rows, &[], 6);
        assert_eq!(drawn(&whole, &rows), vec!["a", "b", "c", "d", "e"]);
        assert_eq!(hidden(&whole), None, "nothing was left out: {whole:?}");

        // One line fewer, and the overflow row costs one of its own: four
        // lines of budget hold three rows and the count of the other two.
        let cut = folded(&rows, &[], 5);
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
        let lines = folded(&rows, &[], 5);
        assert_eq!(drawn(&lines, &rows), vec!["a", "b"]);
        assert_eq!(headers(&lines), vec![("one", false)]);
        assert_eq!(hidden(&lines), Some(2));

        // Seven lines fit both headers, all four rows and the create row.
        let whole = folded(&rows, &[], 7);
        assert_eq!(drawn(&whole, &rows), vec!["a", "b", "c", "d"]);
        assert_eq!(headers(&whole), vec![("one", false), ("two", false)]);
        assert_eq!(hidden(&whole), None);
    }

    /// Eight boring rows of one plain host, in display order, with the five
    /// most recently active of them scattered through it.
    ///
    /// Three layouts disagree about this fixture, which is what makes it worth
    /// building: keeping the first five leaves `s-8 s-7 s-6 s-5 s-4`, drawing
    /// the survivors in the order recency selected them leaves `s-1 s-2 s-3
    /// s-5 s-7`, and the rule is neither.
    fn scattered_rows() -> Vec<SidebarRow> {
        vec![
            row("s-8").active(62).build(),
            row("s-7").active(5).build(),
            row("s-6").active(61).build(),
            row("s-5").active(4).build(),
            row("s-4").active(60).build(),
            row("s-3").active(3).build(),
            row("s-2").active(2).build(),
            row("s-1").active(1).build(),
        ]
    }

    /// The cap keeps a group's most recently active rows and leaves them
    /// exactly where the group had them, and one line says how many it is
    /// holding back.
    #[test]
    fn the_cap_selects_by_recency_and_draws_in_place() {
        let rows = scattered_rows();
        let lines = folded(&rows, &[], 20);
        assert_eq!(
            drawn(&lines, &rows),
            vec!["s-7", "s-5", "s-3", "s-2", "s-1"],
            "the five most recent rows, in the order the group holds them",
        );
        assert_eq!(
            folds(&lines),
            vec![(None, 3)],
            "and the group says how many it is holding: {lines:?}",
        );
        assert_eq!(
            hidden(&lines),
            None,
            "nothing here was cut by the height: {lines:?}",
        );
    }

    /// A group with no more than the cap's worth of boring rows draws no fold
    /// line: there is nothing behind it, and an affordance that opens nothing
    /// is noise on every quiet strip.
    #[test]
    fn a_group_inside_the_cap_draws_no_fold_line() {
        let rows = rows_named(&["a", "b", "c", "d", "e"]);
        let lines = folded(&rows, &[], 20);
        assert_eq!(drawn(&lines, &rows), vec!["a", "b", "c", "d", "e"]);
        assert!(folds(&lines).is_empty(), "{lines:?}");
    }

    /// The cap never holds back a row the strip has promised to show: the
    /// focused row and the ones the client holds open (spec 9.2), and the ones
    /// wearing the working or attention glyph (spec 6.8).
    #[test]
    fn the_cap_never_holds_back_an_exempt_row() {
        for (what, exempt) in [
            ("the focused row", row("s-0").focused()),
            ("a row the client holds open", row("s-0").attached()),
            ("a working row", row("s-0").status(RowStatus::Working)),
            (
                "a row with unseen output",
                row("s-0").status(RowStatus::Unseen),
            ),
        ] {
            // The exempt row is by far the least recently active in the group,
            // so it is the first thing a cap that ignored its exemption would
            // cut, and six boring rows leave the cap something to hold back.
            let mut rows = vec![exempt.active(500).build()];
            rows.extend((1..=6).map(|n| row(&format!("s-{n}")).active(n).build()));
            let lines = folded(&rows, &[], 20);
            assert!(
                drawn(&lines, &rows).contains(&"s-0"),
                "{what} survived the cap: {lines:?}",
            );
            assert_eq!(
                folds(&lines),
                vec![(None, 1)],
                "{what}: the boring row past the cap is what folded: {lines:?}",
            );
        }
    }

    /// A row on a host the peer cannot reach is boring. Its glyph says the peer
    /// cannot answer for it at all, so there is no live signal to suppress, and
    /// a host that went out holding two dozen sessions is exactly the crowd the
    /// cap is for.
    #[test]
    fn the_cap_binds_a_host_that_has_gone_out() {
        let rows: Vec<SidebarRow> = (1..=8)
            .map(|n| {
                row(&format!("s-{n}"))
                    .host("laptop")
                    .status(RowStatus::Unreachable)
                    .active(n)
                    .build()
            })
            .collect();
        // A second host, so the strip wears headers at all and the mark on
        // this one can be read (one host is not a grouping).
        let lines = folded(&rows, &[learned("zeta", false)], 20);
        assert_eq!(drawn(&lines, &rows).len(), 5, "{lines:?}");
        assert_eq!(folds(&lines), vec![(Some("laptop"), 3)], "{lines:?}");
        assert_eq!(
            headers(&lines),
            vec![("laptop", true), ("zeta", false)],
            "and the header still says the host cannot be reached",
        );
    }

    /// Unfolding a group shows every row it was holding, and its line stays
    /// behind as what folds them away again: the pointer has to be able to
    /// undo what the pointer did (spec 9.2).
    #[test]
    fn unfolding_a_group_shows_its_tail_and_keeps_the_line() {
        let rows = scattered_rows();
        let lines = unfolded(&rows, &[], None, 20);
        assert_eq!(
            drawn(&lines, &rows),
            vec!["s-8", "s-7", "s-6", "s-5", "s-4", "s-3", "s-2", "s-1"],
            "every row, still in the order the group holds them",
        );
        assert_eq!(
            folds(&lines),
            vec![(None, 0)],
            "and the line is still there, holding nothing: {lines:?}",
        );

        // A group with nothing to hold back keeps the line too once it has
        // been unfolded, or the gesture would have no way back.
        let quiet = rows_named(&["a", "b"]);
        assert_eq!(folds(&unfolded(&quiet, &[], None, 20)), vec![(None, 0)]);
    }

    /// Each group is capped on its own, and unfolding one leaves the others
    /// exactly as they were. Bounding each host's share is the whole point: a
    /// single budget shared between them is what let one host bury the rest.
    #[test]
    fn the_cap_binds_each_group_on_its_own() {
        let mut rows: Vec<SidebarRow> = (1..=8)
            .map(|n| row(&format!("b-{n}")).host("builder-1").active(n).build())
            .collect();
        rows.extend((1..=7).map(|n| row(&format!("l-{n}")).host("laptop").active(n).build()));
        let lines = folded(&rows, &[], 30);
        assert_eq!(
            folds(&lines),
            vec![(Some("builder-1"), 3), (Some("laptop"), 2)],
            "one line per group, each counting its own: {lines:?}",
        );
        assert_eq!(drawn(&lines, &rows).len(), 10, "{lines:?}");

        let lines = unfolded(&rows, &[], Some("builder-1"), 30);
        assert_eq!(
            folds(&lines),
            vec![(Some("builder-1"), 0), (Some("laptop"), 2)],
            "the unfolded group opened and the other did not: {lines:?}",
        );
        assert_eq!(drawn(&lines, &rows).len(), 13, "{lines:?}");
    }

    /// The fold line costs a line like a header does, and the strip stays
    /// inside its height with the create row on the last line whatever the cap
    /// is doing.
    ///
    /// Where a row and its group's chrome cannot both fit, the row is what
    /// goes: a strip that overran its height would paint over the transcript
    /// beside it, and a group drawing a row without its fold line would hide
    /// rows in silence.
    #[test]
    fn the_fold_line_is_paid_for_out_of_the_height() {
        let rows = scattered_rows();
        for height in 0..=12 {
            let lines = folded(&rows, &[], height);
            assert!(
                lines.len() <= usize::from(height),
                "{height} lines held {lines:?}",
            );
            if height > 0 {
                assert_eq!(
                    lines.last(),
                    Some(&StripLine::New),
                    "{height} lines put the create row last: {lines:?}",
                );
            }
        }

        // Eight lines: the create row, the fold line, the overflow count and
        // the five rows the cap left.
        let whole = folded(&rows, &[], 8);
        assert_eq!(drawn(&whole, &rows).len(), 5, "{whole:?}");
        assert_eq!(folds(&whole), vec![(None, 3)], "{whole:?}");
    }

    /// The two counts answer different questions and are drawn as two lines:
    /// the group's fold line says what the cap is holding, the overflow count
    /// says what the height cut.
    #[test]
    fn the_fold_count_and_the_height_cut_are_counted_apart() {
        // Four rows the client holds open and six boring ones, so the cap
        // holds one back and nine rows want to be drawn.
        let mut rows: Vec<SidebarRow> = (1..=4)
            .map(|n| row(&format!("open-{n}")).attached().active(n).build())
            .collect();
        rows.extend((1..=6).map(|n| row(&format!("s-{n}")).active(n).build()));
        let lines = folded(&rows, &[], 6);
        assert_eq!(
            folds(&lines),
            vec![(None, 1)],
            "the cap is holding one row: {lines:?}",
        );
        assert_eq!(
            hidden(&lines),
            Some(6),
            "and the height cut six more of the nine: {lines:?}",
        );
        assert_eq!(drawn(&lines, &rows).len(), 3, "{lines:?}");
        assert_eq!(lines.len(), 6, "which is every line it had: {lines:?}");
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
        let lines = folded(&rows, &[], 4);
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
            drawn(&folded(&rows, &[], 4), &rows),
            vec!["a", "b"],
            "the row below the focused one fills the line it left",
        );
        rows[0].focused = false;
        rows[1].focused = true;
        assert_eq!(drawn(&folded(&rows, &[], 4), &rows), vec!["a", "b"]);
    }

    /// The minted id every layout test reads its time of day out of.
    const MINTED: &str = "2026-08-06-19-07-19-368";

    /// The width the paint goldens below are written at, carried by the
    /// strip's own state.
    ///
    /// Deliberately not the shipped default: these goldens are about what the
    /// strip's columns say at a width it was told to take, and a literal that
    /// followed the default would have to be re-padded whenever taste moved
    /// it. What the default paints is pinned in the composed shell's tests,
    /// which draw the real layout.
    const PAINT_COLS: u16 = 24;

    /// A draw context with no width at all, which is what a flex row hands an
    /// inflexible child while it measures how wide that child wants to be.
    fn measure_ctx(height: u16) -> DrawContext {
        DrawContext {
            max: MaxSize {
                width: None,
                height: Some(height),
            },
            ..paint_ctx(height)
        }
    }

    /// A draw context roomier than the strip.
    ///
    /// So the paint reads the width the strip carries rather than the one it
    /// was handed: the composed layout measures an inflexible child under an
    /// unbounded width, and a context that exactly fit would paint the same
    /// whether or not the strip knows how wide it is.
    fn paint_ctx(height: u16) -> DrawContext {
        crate::test_support::draw_ctx(PAINT_COLS * 2, Some(height))
    }

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
        // Ten columns is what a nineteen-column field leaves a tag, and a
        // tag that fills them exactly is not cut.
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

    /// The host id a gateway qualifies its rows with, the length production
    /// mints (32 hexadecimal characters), so the assertions below are about a
    /// qualifier that really does swamp the field rather than a short stand-in.
    const HOST: &str = "c6b6667d8f73e75d168afe4f882b0b8b";

    /// A gateway addresses a session as `<host>:<session>`, and the label is
    /// about the session, so the qualifier is not part of what a row shows.
    /// Without this the whole field goes to the qualifier's tail and every row
    /// on every gateway-fronted host reads as a slice of hex.
    #[test]
    fn a_gateway_qualified_id_still_shows_its_time_of_day() {
        let qualified = row(&format!("{HOST}:{MINTED}")).host(HOST).build();
        assert_eq!(qualified.label(19), field("19-07-19", 19));
        // And with a tag beside it, since that is the narrower column.
        assert_eq!(
            row(&format!("{HOST}:{MINTED}"))
                .host(HOST)
                .tag("fix-auth")
                .build()
                .label(19),
            "19-07-19 fix-auth  ",
        );
    }

    /// The qualifier is matched from the host the row carries into the id, and
    /// never looked for in the id, which a client may not parse (spec 6.2). So
    /// an id this row's host does not account for keeps the label it has today,
    /// whole, rather than losing a slice of it to a guess.
    ///
    /// The cases with a colon in them are what tells the two apart: a parse
    /// would find a separator in every one of them and show a time of day. The
    /// id that begins with this host and is separated further along tells the
    /// match apart from a hunt for the separator after the prefix.
    #[test]
    fn a_host_that_does_not_qualify_the_id_labels_from_the_whole_id() {
        // Someone else's qualifier, this host's without its separator, the host
        // alone, a qualifier with nothing after it, and a longer host's whose
        // name this one is a prefix of.
        let foreign = format!("other-host:{MINTED}");
        let unseparated = format!("{HOST}{MINTED}");
        let empty_session = format!("{HOST}:");
        let longer_host = format!("{HOST}x:{MINTED}");
        for id in [
            MINTED,
            "notes-on-the-rust-borrow-checker-draft",
            foreign.as_str(),
            unseparated.as_str(),
            HOST,
            empty_session.as_str(),
            longer_host.as_str(),
        ] {
            assert_eq!(
                row(id).host(HOST).build().label(19),
                field(&session_label(id, 19), 19),
                "{id:?} did not label from the whole id",
            );
        }
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

    /// Which end of a host name a header drops is decided by the name's shape:
    /// a path keeps the tail that tells it from its neighbours, a written name
    /// keeps the head its author chose first.
    ///
    /// A narrow field on purpose, and narrower than the default leaves one: a
    /// host's default name is its whole working directory, so what these cases
    /// pin is eliding, and at a field wide enough to hold these names whole
    /// they would pin nothing. The wider field is covered below.
    #[test]
    fn a_path_name_keeps_its_tail_and_a_written_name_keeps_its_head() {
        assert_eq!(
            header_field("~/work/umber/aj", false, 19),
            format!("~/work/umber/aj {}", "─".repeat(3)),
            "a name that fits is not cut at either end",
        );
        assert_eq!(
            header_field("~/work/umber/materialize/src", false, 19),
            "…/materialize/src ─".to_string(),
            "a deeper clone loses its head, and the ellipsis says so",
        );
        assert_eq!(
            header_field("builder-1-extra-long", false, 19),
            "builder-1-extra-… ─".to_string(),
            "a name with no separator in it loses its tail instead",
        );
        assert_eq!(
            gwidth(
                &header_field("~/work/umber/materialize/src", true, 19),
                Method::Unicode,
            ),
            19,
            "and an elided path still rules out to the edge",
        );
    }

    /// The same rule at the field the shipped default leaves, which is the
    /// field a user meets.
    ///
    /// Structural rather than a golden string: what has to hold at any width
    /// is that a clone too deep for the field is cut at the head and keeps
    /// the tail, and the width is a setting now, so a literal here would go
    /// stale the first time taste moves the default.
    #[test]
    fn a_deep_clone_loses_its_head_at_the_default_field_too() {
        let cols = label_cols(SIDEBAR_COLS);
        let deep = "~/work/umber/materialize/src";
        assert!(
            width_of(deep) > cols,
            "the name fits {cols} columns whole, so this measures no eliding",
        );
        let drawn = header_field(deep, false, cols);
        assert!(
            drawn.starts_with('\u{2026}') && drawn.contains("materialize/src"),
            "the deep clone lost the tail that tells it apart: {drawn:?}",
        );
        assert_eq!(
            gwidth(&drawn, Method::Unicode),
            u16::try_from(cols).expect("a field within a terminal width"),
            "and an elided path still rules out to the edge: {drawn:?}",
        );
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
        let cells = painted_cells(rows, Vec::new(), 6);
        let text = |line: usize| -> String {
            cells[line]
                .iter()
                .map(|cell| cell.char.grapheme())
                .collect()
        };
        assert_eq!(text(0), " ~ builder-1 ───────── │");
        assert_eq!(text(1), "   s-2                 │");
        assert_eq!(text(2), " ~ laptop ──────── ! ─ │", "the header is marked");
        assert_eq!(text(3), " ! s-1                 │", "and so is the row");
        let styles = styles();
        assert_ne!(styles.text, styles.dim, "the two brightnesses differ");
        assert_eq!(
            cells[3][3].style, styles.text,
            "a row the client holds open is drawn as held open",
        );
        assert_eq!(
            cells[1][3].style, styles.text,
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

    /// A host the peer has spoken to, so it names it by the id its rows carry.
    fn learned(id: &str, unreachable: bool) -> DirectoryHost {
        DirectoryHost {
            id: Some(id.to_string()),
            address: None,
            name: None,
            working_directory: None,
            unreachable,
        }
    }

    /// A host that reports a name for itself, as every host does (spec 6.1).
    fn calling_itself(id: &str, name: &str, unreachable: bool) -> DirectoryHost {
        DirectoryHost {
            name: Some(name.to_string()),
            ..learned(id, unreachable)
        }
    }

    /// A configured host the gateway has never reached: no id to be named by,
    /// its address instead, and no rows of its own (spec 7.1).
    fn configured(address: &str) -> DirectoryHost {
        DirectoryHost {
            id: None,
            address: Some(address.to_string()),
            name: None,
            working_directory: None,
            unreachable: true,
        }
    }

    /// A drawn strip over `rows` and the hosts the peer named beside them,
    /// ready to be asked what a pointer lands on.
    fn strip(rows: Vec<SidebarRow>, hosts: Vec<DirectoryHost>, height: u16) -> SessionSidebar {
        let state = Rc::new(RefCell::new(SidebarState {
            visible: true,
            rows,
            hosts,
            ..SidebarState::default()
        }));
        state.borrow_mut().set_cols(PAINT_COLS);
        let mut strip = SessionSidebar::new(state, styles(), HOVER_BG);
        strip.draw(&paint_ctx(height));
        strip
    }

    /// The strip's painted cells at its own width.
    fn painted_cells(
        rows: Vec<SidebarRow>,
        hosts: Vec<DirectoryHost>,
        height: u16,
    ) -> Vec<Vec<vaxis::cell::Cell>> {
        let mut strip = strip(rows, hosts, height);
        let surface = strip.draw(&paint_ctx(height));
        crate::test_support::flatten(&surface)
    }

    /// The strip's painted lines at its own width.
    fn painted(rows: Vec<SidebarRow>, hosts: Vec<DirectoryHost>, height: u16) -> Vec<String> {
        painted_cells(rows, hosts, height)
            .iter()
            .map(|row| row.iter().map(|cell| cell.char.grapheme()).collect())
            .collect()
    }

    /// The strip draws the width it carries, and the label field is what the
    /// extra columns go to.
    ///
    /// Measured under a context with no width at all, which is the question a
    /// flex row asks an inflexible child: answer it from the context and the
    /// strip is as wide as the terminal. A tag long enough to elide at the
    /// default and not at the wider width is what tells a strip whose field
    /// grew from one that padded its way out to the same size.
    #[test]
    fn the_strip_draws_the_width_it_carries() {
        // Sixteen columns of tag: past the fourteen the default field leaves
        // one, inside the twenty-six a strip twelve columns wider leaves it.
        let tag = "rewrite-the-auth";
        let state = Rc::new(RefCell::new(SidebarState {
            visible: true,
            rows: vec![row(MINTED).tag(tag).focused().build()],
            ..SidebarState::default()
        }));
        let mut strip = SessionSidebar::new(Rc::clone(&state), styles(), HOVER_BG);
        // The row itself, out of a strip tall enough to hold it and the create
        // line under it.
        let line = |strip: &mut SessionSidebar| -> String {
            let surface = strip.draw(&measure_ctx(2));
            crate::test_support::flatten(&surface)
                .iter()
                .map(|row| row.iter().map(|cell| cell.char.grapheme()).collect())
                .find(|line: &String| line.contains("19-07-19"))
                .expect("the focused row is painted")
        };

        let shipped = line(&mut strip);
        assert_eq!(
            width_of(&shipped),
            usize::from(SIDEBAR_COLS),
            "an unconfigured strip is the strip the app ships: {shipped:?}",
        );
        assert!(
            shipped.ends_with(SEPARATOR) && shipped.contains('\u{2026}'),
            "the rule closes the strip and the tag elides in it: {shipped:?}",
        );

        state.borrow_mut().set_cols(SIDEBAR_COLS + 12);
        let wider = line(&mut strip);
        assert_eq!(
            width_of(&wider),
            usize::from(SIDEBAR_COLS) + 12,
            "the configured width is the drawn width: {wider:?}",
        );
        assert!(
            wider.ends_with(SEPARATOR) && wider.contains(tag),
            "and the columns went to the field, which now holds the whole \
             tag: {wider:?}",
        );
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
            painted(rows, Vec::new(), 7),
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

    /// The same strip over what a gateway actually sends: ids qualified with a
    /// 32-character host id, which is the shape every row has through the front
    /// door. The rows come off wire summaries rather than being built by hand,
    /// so this covers the derivation together with the host reaching the row.
    /// The composed frame above the widget is not in it, see the strip's other
    /// paint tests.
    #[test]
    fn the_strip_draws_a_gateways_qualified_rows_by_their_time_of_day() {
        let mut tagged = at(&format!("{HOST}:{MINTED}"), 0);
        tagged.tag = Some("vasari".to_string());
        tagged.host = Some(HOST.to_string());
        let mut plain = at(&format!("{HOST}:2026-08-06-18-40-49-001"), 1);
        plain.host = Some(HOST.to_string());
        let rows = rows_for_display(
            &[tagged, plain],
            &format!("{HOST}:{MINTED}"),
            |_| false,
            |_| false,
            false,
        );
        assert_eq!(
            painted(rows, Vec::new(), 4),
            vec![
                "▌  19-07-19 vasari     │",
                "   18-40-49            │",
                " + new                 │",
                "                       │",
            ],
        );
    }

    /// A host the peer holds no rows for, as the strip paints it: a header
    /// with nothing under it, in the place its label sorts it to among the
    /// hosts that have rows, named by the id where the peer has learned one
    /// and by the configured address until it has (spec 7.1).
    ///
    /// The mark rides in the header's rule exactly as it does over a group
    /// whose rows are all unreachable, because it says the same thing: nothing
    /// here can be reached. What the empty group adds is that the contents are
    /// unknown, which is what having no rows under it says.
    #[test]
    fn the_strip_paints_a_host_it_holds_no_rows_for() {
        let rows = vec![
            row("2026-08-06-19-07-19-368")
                .tag("fix-auth")
                .host("builder-1")
                .focused()
                .attached()
                .build(),
        ];
        let hosts = vec![
            learned("builder-1", false),
            learned("laptop", true),
            configured("10.0.0.7:7777"),
        ];
        assert_eq!(
            painted(rows, hosts, 6),
            vec![
                " ~ 10.0.0.7:7777 ─ ! ─ │",
                " ~ builder-1 ───────── │",
                "▌  19-07-19 fix-auth   │",
                " ~ laptop ──────── ! ─ │",
                " + new                 │",
                "                       │",
            ],
        );
    }

    /// The two counted lines read differently, because they count different
    /// things: a group's fold line wears a triangle pointing at the rows it
    /// holds, the strip's overflow count is the plain "…n more" above the
    /// create row. A user reading one for the other would click a line that
    /// does nothing, or wait for a fold that never comes.
    ///
    /// What the line looks like once the group is open is pinned where the
    /// click opens it, in the shell's own pointer tests.
    #[test]
    fn a_fold_line_reads_differently_from_the_overflow_count() {
        // Six lines: three rows, the fold line, the count of the two the
        // height cut, and the create row.
        assert_eq!(
            painted(scattered_rows(), Vec::new(), 6),
            [
                "   s-7                 │",
                "   s-5                 │",
                "   s-3                 │",
                " ▸ 3 more              │",
                "   …2 more             │",
                " + new                 │",
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
        assert_eq!(painted(rows, Vec::new(), 2)[0], "▌* s-1      busy       │");
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
        let cells = painted_cells(rows, Vec::new(), 5);
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
        let cells = painted_cells(rows, Vec::new(), 4);
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

    /// A revealed archived row is struck through, and keeps the brightness its
    /// place in the working set earned. Two axes, two encodings: the strike
    /// says the user is done with the session, the brightness says the client
    /// still holds it open, and neither answer is readable off the other.
    #[test]
    fn a_revealed_archived_row_draws_struck_through() {
        let rows = vec![
            row("2026-08-06-19-07-19-368").focused().archived().build(),
            row("2026-08-06-18-40-49-001").attached().archived().build(),
            row("2026-08-06-17-01-02-002").build(),
        ];
        let cells = painted_cells(rows, Vec::new(), 5);
        let styles = styles();
        // The label field starts at column 3: marker, glyph, space.
        assert!(
            cells[0][3].style.strikethrough,
            "the archived row on screen draws like any other",
        );
        assert_eq!(
            cells[0][3].style,
            Style {
                strikethrough: true,
                ..styles.accent
            },
            "the focused row lost its brightness to the strike",
        );
        assert_eq!(
            cells[1][3].style,
            Style {
                strikethrough: true,
                ..styles.text
            },
            "an archived row the client holds open reads as unheld",
        );
        assert_eq!(
            cells[2][3].style, styles.dim,
            "a row nobody archived came out struck through",
        );
    }

    /// A drawn strip that records every gesture it resolves.
    fn wired(
        rows: Vec<SidebarRow>,
        height: u16,
    ) -> (SessionSidebar, Rc<RefCell<Vec<StripGesture>>>) {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let mut strip = strip(rows, Vec::new(), height);
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
        strip.draw(&paint_ctx(height));
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
        // Two hosts, so the strip wears headers: `~ builder-1`, its two rows,
        // `~ laptop`, its two rows, then the create row.
        let rows = vec![
            row("laptop-b").host("laptop").focused().build(),
            row("laptop-a").host("laptop").build(),
            row("builder-b").host("builder-1").build(),
            row("builder-a").host("builder-1").build(),
        ];
        let (mut strip, seen) = wired(rows, 8);
        for line in [1, 2, 4, 5] {
            let ctx = deliver(&mut strip, &press(line));
            assert!(ctx.consume_event, "line {line} was acted on");
        }
        assert_eq!(
            *seen.borrow(),
            focus_of(&["builder-b", "builder-a", "laptop-b", "laptop-a"]),
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
    /// between sees rows that have already moved: a session created or ended
    /// on any host shifts the rows below it by one.
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

    /// A click on a fold line names the group it belongs to, and nothing else.
    /// The gesture is the pointer's trigger for the fold action, so it carries
    /// the same thing the chord works out from the focused row.
    #[test]
    fn a_click_on_a_fold_line_names_its_group() {
        let mut rows: Vec<SidebarRow> = (1..=8)
            .map(|n| row(&format!("b-{n}")).host("builder-1").active(n).build())
            .collect();
        rows.extend((1..=8).map(|n| row(&format!("l-{n}")).host("laptop").active(n).build()));
        // A header, five rows and a fold line per group, then the create row.
        let (mut strip, seen) = wired(rows, 15);
        let ctx = deliver(&mut strip, &press(6));
        assert!(ctx.consume_event, "the fold line acted on the press");
        deliver(&mut strip, &press(13));
        assert_eq!(
            *seen.borrow(),
            vec![
                StripGesture::Fold(Some("builder-1".to_string())),
                StripGesture::Fold(Some("laptop".to_string())),
            ],
            "each line named its own group",
        );

        // A plain host's rows sit in the unlabeled group, which folds like any
        // other and has no name to be asked for.
        let (mut strip, seen) = wired(scattered_rows(), 15);
        deliver(&mut strip, &press(5));
        assert_eq!(*seen.borrow(), vec![StripGesture::Fold(None)]);
    }

    /// The fold line wears the hover band, because a click on it does
    /// something. The band marks what a click acts on and nothing else.
    #[test]
    fn the_band_marks_a_fold_line() {
        let (mut strip, _) = wired(scattered_rows(), 10);
        deliver(&mut strip, &motion(5));
        assert_eq!(
            banded(&painted_bgs(&mut strip, 10)),
            vec![5],
            "the fold line under the pointer is banded",
        );
    }

    /// The background of each painted line's first label cell.
    fn painted_bgs(strip: &mut SessionSidebar, height: u16) -> Vec<Color> {
        let surface = strip.draw(&paint_ctx(height));
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
        // Attached, so the cap holds none of them back and the height is what
        // decides which rows are on screen (see [`GROUP_CAP`]).
        let rows: Vec<SidebarRow> = ids.iter().map(|id| row(id).attached().build()).collect();
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
        let mut rows: Vec<SidebarRow> = ids.iter().map(|id| row(id).attached().build()).collect();
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
                    // Attached, so every row stays on screen and the wheel is
                    // the only thing moving the run (see [`GROUP_CAP`]).
                    let built = row(id).attached();
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
