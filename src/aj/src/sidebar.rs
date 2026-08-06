//! The session sidebar: a strip listing every session the peer offers
//! (spec 9.2).
//!
//! The widget is read-only chrome. It renders from a [`SidebarState`] mirror
//! the drive loop refreshes once per iteration from the client's session
//! directory, the same single-writer arrangement the status chrome uses. The
//! widget cannot reach the directory itself (that lives behind the world), and
//! mirroring keeps the draw path free of any borrow the loop also wants.
//!
//! Hidden by default. A lone local session has nothing to choose between, so
//! the strip would be chrome that costs width and says nothing.

use std::cell::RefCell;
use std::rc::Rc;

use aj_wire::SessionSummary;
use vaxis::vxfw::{DrawContext, MaxSize, RichText, Size, Surface, TextSpan, Widget};

use crate::transcript::TranscriptStyles;

/// Columns the sidebar occupies when shown, glyph and padding included.
///
/// Fixed rather than proportional: session ids are a timestamp of known width,
/// and a strip that grew with the terminal would take space from the transcript
/// for nothing. Wide enough for `HH-MM-SS` plus the glyph and a space, which is
/// what distinguishes one of a day's sessions from another.
pub(crate) const SIDEBAR_COLS: u16 = 14;

/// What a row's glyph says about its session, in the order a row that could
/// claim several of these should claim one (spec 6.8, 9.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RowStatus {
    /// The peer cannot reach the host this session lives on. Says nothing about
    /// the session itself, which is why it outranks the rest.
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
    /// Whether the strip is drawn at all.
    pub(crate) visible: bool,
    pub(crate) rows: Vec<SidebarRow>,
}

impl SidebarState {}

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
    /// Ids are `YYYY-MM-DD-HH-MM-SS-mmm`, and the date is the same for every
    /// session the user is likely to be juggling, so the leading date earns none
    /// of the width it costs. An id in any other shape (a hand-renamed file) is
    /// shown from its tail, where what distinguishes it usually is.
    ///
    /// NOTE: two sessions minted in the same second therefore share a label.
    /// The strip is for orientation, not identification, and the header carries
    /// the focused session's id in full.
    pub(crate) fn label(id: &str, width: usize) -> String {
        let parts: Vec<&str> = id.split('-').collect();
        let label = if parts.len() >= 7 {
            parts[3..6].join("-")
        } else {
            id.to_string()
        };
        if label.chars().count() <= width {
            return label;
        }
        label.chars().skip(label.chars().count() - width).collect()
    }
}

impl Widget for SessionSidebar {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let state = self.state.borrow();
        if !state.visible {
            return Surface::with_size(Size {
                width: 0,
                height: 0,
            });
        }
        // A flex row measures its inflexible children under an unbounded width,
        // so the width has to come from this widget rather than the context:
        // that measurement is exactly the question "how wide are you". Under a
        // bounded context we still take the narrower of the two, so a terminal
        // too narrow for the strip does not overflow it.
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
        // One column of glyph, one of separator, the rest for the label.
        let label_width = usize::from(width.saturating_sub(2));
        let rows: Box<dyn Iterator<Item = &SidebarRow>> = match ctx.max.height {
            Some(height) => Box::new(state.rows.iter().take(usize::from(height))),
            None => Box::new(state.rows.iter()),
        };
        let mut spans: Vec<TextSpan> = Vec::with_capacity(state.rows.len() * 2);
        for (index, row) in rows.enumerate() {
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

    /// A collision suffix rides the id's last component, so the id still reads
    /// as the minted shape and still shows its time of day. Two sessions a
    /// millisecond apart therefore share a label, which the strip accepts: it
    /// is for orientation, and the header names the focused session in full.
    #[test]
    fn a_collision_suffix_still_reads_as_a_minted_id() {
        assert_eq!(
            SessionSidebar::label("2026-08-06-19-07-19-368_2", 12),
            "19-07-19",
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
}
