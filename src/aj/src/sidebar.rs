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
//! there and you do not have it open.
//!
//! Layout is a pure function, [`strip_lines`], producing one [`StripLine`] per
//! drawn line, and drawing is a dumb map over its result. The height
//! arithmetic (host headers, the overflow row and the create row all take
//! lines away from the rows) therefore lives in one testable place, and a
//! pointer gesture can resolve a line by its index without deriving the layout
//! a second time.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use aj_wire::SessionSummary;
use vaxis::cell::Style;
use vaxis::gwidth::{Method, gwidth};
use vaxis::vxfw::{DrawContext, MaxSize, Overflow, RichText, Size, Surface, TextSpan, Widget};

use crate::transcript::TranscriptStyles;

/// Columns the sidebar occupies when shown.
///
/// Fixed rather than proportional: a strip that grew with the terminal would
/// take width from the transcript for nothing. The columns go one to the focus
/// marker, one to the status glyph, one to a space, the rest to the label, and
/// the last two to a pad and the separator rule. Sized so a hand-written tag
/// reads without eliding, which is most of what the strip is for once sessions
/// carry names.
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
    /// An unreachable session counts as merely listed whatever the client
    /// holds for it: the attachment carries nothing while the peer cannot
    /// reach the host, so drawing it as open would overstate what the client
    /// has. Focus still outranks that, because which session is on screen has
    /// to stay legible.
    fn presence(&self) -> Presence {
        if self.focused {
            Presence::Focused
        } else if self.attached && self.status != RowStatus::Unreachable {
            Presence::Background
        } else {
            Presence::Listed
        }
    }

    /// What the row shows: the user's tag where there is one, and the
    /// id-derived label otherwise (spec 9.2).
    fn label(&self, cols: usize) -> String {
        match &self.tag {
            // A tag reads left to right, so an over-long one keeps its head
            // and says so with an ellipsis. An id is the other way round:
            // what distinguishes one is its tail (see [`session_label`]).
            Some(tag) => elide_to_cols(&one_line(tag), cols),
            None => session_label(&self.id, cols),
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

/// Lay the strip out for `rows` in `height` lines.
///
/// The one place the height arithmetic lives. Host headers, the overflow row
/// and the create row all take lines away from the rows, and the focused row
/// has to survive that. Never returns more than `height` lines.
///
/// A height that cannot fit even the focused row and its header (two lines of
/// strip, with hosts to group) gives up on showing a row rather than
/// overrunning: the honest answer at that size is the overflow count.
pub(crate) fn strip_lines(rows: &[SidebarRow], height: u16) -> Vec<StripLine> {
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
    layout.lines(layout.visible_run(budget))
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
    /// that fits with the focused row inside it.
    ///
    /// Scrolls by the least it can. The run stays anchored at the top until
    /// focus would fall past its bottom edge, which keeps a step from jumping
    /// the whole strip. With no focused row it shows the top, which is the
    /// most recently active end.
    fn visible_run(&self, budget: usize) -> Range<usize> {
        let total = self.order.len();
        // Scanned downward because cost is not monotone in the run's length:
        // the run holding every row spends no line on the overflow row, so it
        // can fit where a run one row shorter does not.
        let top = (0..=total)
            .rev()
            .find(|&end| self.cost(0..end) <= budget)
            .unwrap_or(0);
        let Some(focus) = self
            .order
            .iter()
            .position(|&index| self.rows[index].focused)
        else {
            return 0..top;
        };
        if focus < top {
            return 0..top;
        }
        // Focus fell past the bottom edge, so the run ends on it and reaches
        // back as far as the budget allows. An empty run is the answer when
        // even one row and its header will not fit (see [`strip_lines`]).
        (0..=focus)
            .find(|&start| self.cost(start..focus + 1) <= budget)
            .map_or(focus..focus, |start| start..focus + 1)
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
    let cleaned = one_line(id);
    let parts: Vec<&str> = cleaned.split('-').collect();
    // The minted shape, not merely something with seven dashes: a hand-named
    // session can have as many, and slicing its middle out would show a
    // fragment of a word.
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
    truncate_to_cols(&label, cols)
}

/// Drop control characters, which a session's name, a user's tag and a peer's
/// host name may all contain and which would split the row's line and
/// misattribute every row below it.
fn one_line(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
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
    let width = usize::from(gwidth(&out, Method::Unicode));
    out.push_str(&" ".repeat(cols.saturating_sub(width)));
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
    let name_cols = usize::from(gwidth(&name, Method::Unicode));
    let rule = cols.saturating_sub(name_cols + 1 + mark_cols);
    format!("{name} {}{mark}", "─".repeat(rule))
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
    fn line_spans(&self, line: &StripLine, rows: &[SidebarRow], width: u16) -> Vec<TextSpan> {
        let cols = label_cols(width);
        let dim = self.styles.dim;
        let (marker, glyph, glyph_style, label, label_style) = match line {
            StripLine::Header { host, unreachable } => {
                (" ", "~", dim, header_field(host, *unreachable, cols), dim)
            }
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
            StripLine::Overflow { hidden } => (" ", " ", dim, format!("…{hidden} more"), dim),
            StripLine::New => (" ", "+", dim, "new".to_string(), dim),
        };
        vec![
            span(marker, self.styles.accent),
            span(glyph, glyph_style),
            span(&format!(" {}", field(&label, cols)), label_style),
            span(&format!(" {SEPARATOR}"), dim),
        ]
    }

    /// A line below the last drawn one: nothing but the separator, which runs
    /// the strip's full height so the transcript's edge is one unbroken rule.
    fn blank_spans(&self, width: u16) -> Vec<TextSpan> {
        let pad = " ".repeat(usize::from(width) - 1);
        vec![span(&format!("{pad}{SEPARATOR}"), self.styles.dim)]
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
        // An unbounded height is a measurement pass, which reads back only the
        // width: lay every line out and let the surface be as tall as it is.
        let lines = strip_lines(&state.rows, ctx.max.height.unwrap_or(u16::MAX));
        let blanks = usize::from(ctx.max.height.unwrap_or(0)).saturating_sub(lines.len());
        let mut spans: Vec<TextSpan> = Vec::with_capacity((lines.len() + blanks) * 5);
        for index in 0..lines.len() + blanks {
            if index > 0 {
                spans.push(span("\n", self.styles.text));
            }
            match lines.get(index) {
                Some(line) => spans.extend(self.line_spans(line, &state.rows, width)),
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
        let tagged = row("session-1").tag("first\nsecond").build();
        assert!(
            !tagged.label(12).contains('\n'),
            "{:?} would break the row",
            tagged.label(12),
        );
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
            let lines = strip_lines(&rows, height);
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
        assert!(strip_lines(&rows_named(&["a", "b"]), 0).is_empty());
    }

    /// One host, or none at all, is not a grouping. A plain connect has to look
    /// exactly as it would have before hosts existed (spec 9.2).
    #[test]
    fn a_single_host_gets_no_headers() {
        let hostless = rows_named(&["a", "b", "c"]);
        assert!(
            headers(&strip_lines(&hostless, 20)).is_empty(),
            "rows with no host name nothing to group under",
        );
        let one_host: Vec<SidebarRow> = ["a", "b", "c"]
            .iter()
            .map(|id| row(id).host("builder-1").build())
            .collect();
        let lines = strip_lines(&one_host, 20);
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
        let lines = strip_lines(&rows, 20);
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
            headers(&strip_lines(&rows, 20)),
            vec![("laptop", true), ("builder-1", false)],
        );
        rows[1].status = RowStatus::Idle;
        assert_eq!(
            headers(&strip_lines(&rows, 20)),
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
        let lines = strip_lines(&rows, 20);
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
        let whole = strip_lines(&rows, 6);
        assert_eq!(drawn(&whole, &rows), vec!["a", "b", "c", "d", "e"]);
        assert_eq!(hidden(&whole), None, "nothing was left out: {whole:?}");

        // One line fewer, and the overflow row costs one of its own: four
        // lines of budget hold three rows and the count of the other two.
        let cut = strip_lines(&rows, 5);
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
        let lines = strip_lines(&rows, 5);
        assert_eq!(drawn(&lines, &rows), vec!["a", "b"]);
        assert_eq!(headers(&lines), vec![("one", false)]);
        assert_eq!(hidden(&lines), Some(2));

        // Seven lines fit both headers, all four rows and the create row.
        let whole = strip_lines(&rows, 7);
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
        let lines = strip_lines(&rows, 4);
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
            drawn(&strip_lines(&rows, 4), &rows),
            vec!["a", "b"],
            "the row below the focused one fills the line it left",
        );
        rows[0].focused = false;
        rows[1].focused = true;
        assert_eq!(drawn(&strip_lines(&rows, 4), &rows), vec!["a", "b"]);
    }

    /// A row shows the name the user gave it, and an over-long one keeps its
    /// head and says it was cut.
    #[test]
    fn a_tag_is_shown_in_place_of_the_id() {
        let tagged = row("2026-08-06-19-07-19-368").tag("fix-auth").build();
        assert_eq!(tagged.label(19), "fix-auth");
        assert_eq!(
            row("2026-08-06-19-07-19-368").build().label(19),
            "19-07-19",
            "an untagged row falls back to the id-derived label",
        );
        let long = row("session-1")
            .tag("rewrite-the-gateway-provisioner")
            .build();
        assert_eq!(long.label(19), "rewrite-the-gatewa…");
        assert_eq!(
            gwidth(&long.label(19), Method::Unicode),
            19,
            "the ellipsis is inside the budget, not beside it",
        );
    }

    /// A tag of wide graphemes is budgeted in display columns, so it cannot
    /// overflow the field and push the separator off its column.
    #[test]
    fn a_wide_tag_fits_its_columns() {
        let wide = row("session-1").tag("会話ノート記録帳の下書き").build();
        let label = wide.label(9);
        assert!(
            gwidth(&label, Method::Unicode) <= 9,
            "{label:?} takes {} columns",
            gwidth(&label, Method::Unicode),
        );
        assert!(label.ends_with('…'), "and it says it was cut");
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
        // An attachment to a host the peer cannot reach is carrying nothing.
        assert_eq!(
            row("a")
                .attached()
                .status(RowStatus::Unreachable)
                .build()
                .presence(),
            Presence::Listed,
        );
    }

    fn styles() -> Rc<TranscriptStyles> {
        Rc::new(TranscriptStyles::from_theme(
            &aj_app::theme::Theme::bundled_dark_with_mode(aj_app::theme::ColorMode::Truecolor),
            crate::terminal::TerminalCaps::default(),
        ))
    }

    /// The strip's painted cells at its own width.
    fn painted_cells(rows: Vec<SidebarRow>, height: u16) -> Vec<Vec<vaxis::cell::Cell>> {
        let state = Rc::new(RefCell::new(SidebarState {
            visible: true,
            rows,
            ..SidebarState::default()
        }));
        let mut strip = SessionSidebar::new(state, styles());
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
    /// glyph so the two never contend for one cell, the label field, and the
    /// separator running the strip's full height whether or not there is a line
    /// beside it.
    #[test]
    fn the_strip_draws_its_columns() {
        let rows = vec![
            row("s-1")
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
                "▌  fix-auth            │",
                " * eval-run            │",
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
        assert_eq!(painted(rows, 2)[0], "▌* busy                │");
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
}
