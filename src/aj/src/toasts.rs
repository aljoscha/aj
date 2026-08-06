//! The transient toast stack: small, non-interactive corner boxes that report
//! short-lived facts (a select-to-copy landing, a busy refusal) bottom-right
//! and clear themselves when they expire.
//!
//! All toasts share one stack, so several can be live at once: they render
//! stacked vertically in the same bottom-right spot whether or not a modal
//! overlay is open (the Shell picks the z, this module only builds surfaces).
//! Each toast carries its own timer. A toast has no self-timer widget-side:
//! the drive loop wakes at the earliest deadline, prunes what expired
//! ([`prune_expired`]), and requests the clearing repaint, so every toast
//! vanishes exactly on time even while others stay live.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use aj_app::keybindings::{ACTION_AGENT_PICKER, action_shortcut, fixed_keys};
use vaxis::vxfw::{DrawContext, Size, Surface};

use crate::corner_box::{CornerBoxBody, corner_box, span};
use crate::overlay::OverlayChrome;
use crate::transcript::TranscriptStyles;

/// How long the copy toast stays up: a couple of seconds, matching the
/// quit-arm hint's timeout so the two boxes feel of a piece.
const COPY_TOAST_DURATION: Duration = Duration::from_millis(2000);

/// How long a notice toast stays up. Longer than the copy toast because a
/// busy refusal carries a remedy row the user should get to read.
const NOTICE_TOAST_DURATION: Duration = Duration::from_millis(4000);

/// A styled toast fragment. Semantic kinds rather than resolved `Style`s so a
/// runtime theme swap re-tints a live toast through the widget's current
/// styles.
#[derive(PartialEq, Eq)]
pub(crate) enum ToastSpan {
    /// The accent (key-hint) style, for the value part of a value/label pair.
    Accent(String),
    /// The dim body style.
    Dim(String),
}

/// A toast's body: one or more rows of styled spans. Most toasts are a
/// single row; the busy refusal splits its message and remedy across two so
/// the box stays narrow enough for an 80-column terminal.
pub(crate) struct ToastBody {
    rows: Vec<Vec<ToastSpan>>,
}

impl From<String> for ToastBody {
    fn from(message: String) -> ToastBody {
        ToastBody {
            rows: vec![vec![ToastSpan::Dim(message)]],
        }
    }
}

impl From<&str> for ToastBody {
    fn from(message: &str) -> ToastBody {
        ToastBody::from(message.to_string())
    }
}

/// One raised toast: its styled body rows, when it was raised, and how long
/// it stays up.
pub(crate) struct Toast {
    rows: Vec<Vec<ToastSpan>>,
    raised_at: Instant,
    duration: Duration,
}

impl Toast {
    /// Whether this toast is still within its display window.
    pub(crate) fn is_live(&self) -> bool {
        self.raised_at.elapsed() < self.duration
    }

    /// When this toast expires, for the drive loop's wake scheduling.
    fn deadline(&self) -> Instant {
        self.raised_at + self.duration
    }

    /// The unstyled message text, rows newline-joined, for test assertions.
    #[cfg(test)]
    pub(crate) fn text(&self) -> String {
        self.rows
            .iter()
            .map(|row| row_text(row))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The shared toast stack: hosts push ([`show_toast`], [`push_copy_toast`]),
/// the [`Toasts`] widget draws, and the drive loop prunes and schedules wakes.
pub(crate) type ToastStack = Rc<RefCell<Vec<Toast>>>;

/// Raise a transient notice toast with `body`. Live toasts stack, each
/// with its own timer. The caller still owns the repaint: raise this, then
/// request a redraw so the box appears immediately (the drive loop schedules
/// the clearing repaint at the toast's deadline).
pub(crate) fn show_toast(stack: &ToastStack, body: impl Into<ToastBody>) {
    push_or_refresh(stack, body.into().rows, NOTICE_TOAST_DURATION);
}

/// Raise the "copied to clipboard" toast for a select-to-copy of `chars`
/// characters: the count in the accent style, the rest dimmed, matching the
/// value/label split of the frame-stats and quit-hint boxes.
pub(crate) fn push_copy_toast(stack: &ToastStack, chars: usize) {
    let noun = if chars == 1 {
        "character"
    } else {
        "characters"
    };
    push_or_refresh(
        stack,
        vec![vec![
            ToastSpan::Accent(chars.to_string()),
            ToastSpan::Dim(format!(" {noun} copied to clipboard")),
        ]],
        COPY_TOAST_DURATION,
    );
}

/// Push a toast, or, when a live toast with identical content is already on
/// the stack, refresh that one's timer instead. Without the dedup a held
/// chord (key repeat on a refused gesture) would stack a column of identical
/// boxes, one per repeat.
fn push_or_refresh(stack: &ToastStack, rows: Vec<Vec<ToastSpan>>, duration: Duration) {
    let mut toasts = stack.borrow_mut();
    if let Some(same) = toasts.iter_mut().find(|t| t.is_live() && t.rows == rows) {
        same.raised_at = Instant::now();
        return;
    }
    toasts.push(Toast {
        rows,
        raised_at: Instant::now(),
        duration,
    });
}

/// Drop every expired toast. Returns whether anything was dropped, so the
/// drive loop can request the clearing repaint exactly when one is needed.
pub(crate) fn prune_expired(stack: &ToastStack) -> bool {
    let mut toasts = stack.borrow_mut();
    let before = toasts.len();
    toasts.retain(Toast::is_live);
    toasts.len() != before
}

/// The earliest toast's expiry, for the drive loop's wake deadline. `None`
/// only when the stack is empty.
///
/// Deliberately over every toast still on the stack, expired or not: a toast
/// that expires between the loop's prune and this computation yields a past
/// deadline, so the wake fires immediately and the next prune drops it and
/// requests the clearing repaint. Filtering to live toasts would leave that
/// toast painted, with no wake scheduled, until an unrelated event.
pub(crate) fn earliest_toast_deadline(stack: &ToastStack) -> Option<Instant> {
    stack.borrow().iter().map(Toast::deadline).min()
}

/// The unstyled text of one toast row, for test assertions.
#[cfg(test)]
fn row_text(row: &[ToastSpan]) -> String {
    row.iter()
        .map(|s| match s {
            ToastSpan::Accent(t) | ToastSpan::Dim(t) => t.as_str(),
        })
        .collect()
}

/// The unstyled messages of the live toasts, in stack order, for tests.
#[cfg(test)]
pub(crate) fn toast_texts(stack: &ToastStack) -> Vec<String> {
    stack
        .borrow()
        .iter()
        .filter(|t| t.is_live())
        .map(Toast::text)
        .collect()
}

/// The refusal toast for a session-changing gesture while work is running.
/// Two rows: the message (`"Can't <what> while work is running"`) and the
/// remedy row, split so the box fits an 80-column terminal. One builder so
/// the wording (and the remedy) can't drift across the refuse sites.
pub(crate) fn busy_refusal(what: &str) -> ToastBody {
    ToastBody {
        rows: vec![
            vec![ToastSpan::Dim(format!(
                "Can't {what} while work is running"
            ))],
            vec![ToastSpan::Dim(remedy_row())],
        ],
    }
}

/// The remedy row shared by every busy refusal: how to cancel the running
/// turn and where to stop background tasks. The cancel chord is a fixed
/// terminal convention (no keybinding entry), so it reads from `fixed_keys`;
/// the picker chord resolves through the keybinding data, falling back to the
/// literal if the entry ever disappears.
fn remedy_row() -> String {
    let picker = action_shortcut(ACTION_AGENT_PICKER).unwrap_or_else(|| "Alt+A".to_string());
    format!(
        "{} cancels \u{00b7} {picker} stops tasks",
        fixed_keys::CTRL_C
    )
}

/// The toast-stack widget.
///
/// Styles come from two shared sources so a runtime theme swap re-tints the
/// boxes without rebuilding them: `styles` (the body colors) and `chrome`
/// (the frame border, read live from the cell the Shell also restyles).
/// `toasts` is the shared stack the hosts push into.
pub(crate) struct Toasts {
    styles: Rc<TranscriptStyles>,
    chrome: Rc<RefCell<OverlayChrome>>,
    toasts: ToastStack,
}

impl Toasts {
    pub(crate) fn new(
        styles: Rc<TranscriptStyles>,
        chrome: Rc<RefCell<OverlayChrome>>,
        toasts: ToastStack,
    ) -> Toasts {
        Toasts {
            styles,
            chrome,
            toasts,
        }
    }

    /// Replace the body styles, for a runtime theme swap. The frame styles
    /// live in the shared `chrome` cell and need no push here.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        self.styles = styles;
    }

    /// Build one surface per live toast, in stack order: oldest first, so a
    /// caller placing them bottom-up puts the oldest closest to the bottom.
    ///
    /// `avail` bounds the whole stack. Each drawn toast consumes its height
    /// from the budget, and a toast that doesn't fit the remaining room is
    /// skipped for this frame (its expiry, or a resize, frees the room).
    /// The surfaces are non-interactive; the caller anchors them.
    pub(crate) fn draw_stack(&self, ctx: &DrawContext, avail: Size) -> Vec<Surface> {
        let chrome = self.chrome.borrow();
        let mut remaining = avail.height;
        let mut out = Vec::new();
        for toast in self.toasts.borrow().iter().filter(|t| t.is_live()) {
            let mut spans = Vec::new();
            let mut content_width = 0;
            for (i, row) in toast.rows.iter().enumerate() {
                if i > 0 {
                    spans.push(span("\n".to_string(), self.styles.dim));
                }
                let mut row_width = 0;
                for s in row {
                    let (text, style) = match s {
                        ToastSpan::Accent(t) => (t, self.styles.keybinding_hint),
                        ToastSpan::Dim(t) => (t, self.styles.dim),
                    };
                    row_width += ctx.string_width(text);
                    spans.push(span(text.clone(), style));
                }
                content_width = content_width.max(row_width);
            }
            let Some(surf) = corner_box(
                ctx,
                &chrome,
                Size {
                    width: avail.width,
                    height: remaining,
                },
                CornerBoxBody {
                    title: String::new(),
                    spans,
                    content_width,
                    content_rows: toast.rows.len(),
                },
            ) else {
                continue;
            };
            remaining = remaining.saturating_sub(surf.size.height);
            out.push(surf);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use aj_app::theme::{ColorMode, Theme};

    use super::*;
    use crate::test_support::{draw_ctx, rows};

    fn theme() -> Theme {
        Theme::bundled_dark_with_mode(ColorMode::Truecolor)
    }

    fn empty_stack() -> ToastStack {
        Rc::new(RefCell::new(Vec::new()))
    }

    fn widget_over(stack: &ToastStack) -> Toasts {
        let t = theme();
        Toasts::new(
            Rc::new(TranscriptStyles::from_theme(
                &t,
                crate::terminal::TerminalCaps::default(),
            )),
            Rc::new(RefCell::new(OverlayChrome::from_theme(&t))),
            Rc::clone(stack),
        )
    }

    fn roomy() -> Size {
        Size {
            width: 200,
            height: 50,
        }
    }

    /// Backdate a stack's most recent toast by `by`, to simulate elapsed time
    /// without sleeping.
    fn backdate_last(stack: &ToastStack, by: Duration) {
        stack
            .borrow_mut()
            .last_mut()
            .expect("a toast to backdate")
            .raised_at -= by;
    }

    /// With no toasts on the stack nothing draws.
    #[test]
    fn empty_stack_draws_nothing() {
        let stack = empty_stack();
        let toasts = widget_over(&stack);
        assert!(
            toasts
                .draw_stack(&draw_ctx(200, Some(50)), roomy())
                .is_empty()
        );
    }

    /// An expired toast draws nothing, so the box clears itself on the
    /// repaint the drive loop schedules at its deadline.
    #[test]
    fn expired_toast_draws_nothing() {
        let stack = empty_stack();
        show_toast(&stack, "gone");
        backdate_last(&stack, NOTICE_TOAST_DURATION + Duration::from_millis(1));
        let toasts = widget_over(&stack);
        assert!(
            toasts
                .draw_stack(&draw_ctx(200, Some(50)), roomy())
                .is_empty()
        );
    }

    /// A live notice toast renders its message.
    #[test]
    fn live_notice_reports_the_message() {
        let stack = empty_stack();
        show_toast(&stack, "Can't switch sessions while work is running.");
        let toasts = widget_over(&stack);
        let surfs = toasts.draw_stack(&draw_ctx(200, Some(50)), roomy());
        assert_eq!(surfs.len(), 1);
        let body = rows(&surfs[0]).join("\n");
        assert!(
            body.contains("Can't switch sessions while work is running."),
            "{body:?}"
        );
    }

    /// A live copy toast renders the count and the plural noun.
    #[test]
    fn live_copy_reports_the_count() {
        let stack = empty_stack();
        push_copy_toast(&stack, 42);
        let toasts = widget_over(&stack);
        let surfs = toasts.draw_stack(&draw_ctx(200, Some(50)), roomy());
        assert_eq!(surfs.len(), 1);
        let body = rows(&surfs[0]).join("\n");
        assert!(
            body.contains("42 characters copied to clipboard"),
            "{body:?}"
        );
    }

    /// A single copied character uses the singular noun.
    #[test]
    fn one_character_is_singular() {
        let stack = empty_stack();
        push_copy_toast(&stack, 1);
        let toasts = widget_over(&stack);
        let surfs = toasts.draw_stack(&draw_ctx(200, Some(50)), roomy());
        let body = rows(&surfs[0]).join("\n");
        assert!(body.contains("1 character copied to clipboard"), "{body:?}");
        assert!(!body.contains("characters"), "singular noun: {body:?}");
    }

    /// The boxes' surfaces carry no widget identity, so they never join the
    /// focus path.
    #[test]
    fn box_surfaces_are_non_interactive() {
        let stack = empty_stack();
        show_toast(&stack, "hi");
        push_copy_toast(&stack, 3);
        let toasts = widget_over(&stack);
        for surf in toasts.draw_stack(&draw_ctx(200, Some(50)), roomy()) {
            assert!(surf.widget.is_none(), "the boxes must be non-interactive");
        }
    }

    /// A toast declines when the terminal can't fit the frame plus content,
    /// without blocking the toasts that do fit.
    #[test]
    fn declines_when_it_does_not_fit() {
        let stack = empty_stack();
        show_toast(&stack, "a rather long message that will not fit");
        push_copy_toast(&stack, 7);
        let toasts = widget_over(&stack);
        let ctx = draw_ctx(200, Some(50));
        let surfs = toasts.draw_stack(
            &ctx,
            Size {
                width: 36,
                height: 50,
            },
        );
        assert_eq!(surfs.len(), 1, "only the fitting toast draws");
        let body = rows(&surfs[0]).join("\n");
        assert!(body.contains("copied to clipboard"), "{body:?}");
    }

    /// The stack order is deterministic: surfaces come back oldest first, so
    /// the caller's bottom-up placement puts the oldest closest to the bottom.
    #[test]
    fn draw_stack_returns_oldest_first() {
        let stack = empty_stack();
        show_toast(&stack, "older toast");
        show_toast(&stack, "newer toast");
        let toasts = widget_over(&stack);
        let surfs = toasts.draw_stack(&draw_ctx(200, Some(50)), roomy());
        assert_eq!(surfs.len(), 2);
        assert!(
            rows(&surfs[0]).join("\n").contains("older toast"),
            "oldest first"
        );
        assert!(rows(&surfs[1]).join("\n").contains("newer toast"));
    }

    /// Per-toast expiry: with two toasts raised apart, the wake deadline
    /// tracks the earliest toast on the stack (here the already-expired copy
    /// toast, whose past deadline fires the wake immediately), and the prune
    /// at that wake drops only the expired toast (returning `true` so the
    /// drive loop repaints) while the later stays live with its own later
    /// deadline, which then drives the next wake.
    #[test]
    fn earlier_toast_expires_and_prunes_while_later_stays_live() {
        let stack = empty_stack();
        // The copy toast (2s window) backdated just past its deadline, then a
        // notice raised ~1.9s later in wall-clock terms, still live.
        push_copy_toast(&stack, 5);
        backdate_last(&stack, COPY_TOAST_DURATION + Duration::from_millis(1));
        show_toast(&stack, "still live");

        // The wake deadline is the expired copy toast's, already in the past.
        let copy_deadline = stack.borrow()[0].deadline();
        assert_eq!(earliest_toast_deadline(&stack), Some(copy_deadline));
        assert!(copy_deadline <= Instant::now(), "an immediate wake");

        // The prune drops exactly the expired copy toast and reports it, so
        // the drive loop requests the clearing repaint while the notice stays.
        assert!(prune_expired(&stack), "the expired toast was pruned");
        assert_eq!(toast_texts(&stack), vec!["still live".to_string()]);
        assert!(!prune_expired(&stack), "nothing left to prune");

        // The next wake tracks the surviving notice's deadline.
        let notice_deadline = stack.borrow()[0].deadline();
        assert_eq!(earliest_toast_deadline(&stack), Some(notice_deadline));
    }

    /// A toast that expires between the loop's prune and its deadline
    /// computation still yields a wake deadline (a past one), so the next
    /// wake fires immediately and prunes it. `None` would disable the sleep
    /// arm and strand the painted toast until an unrelated event.
    #[test]
    fn expired_unpruned_toast_still_yields_a_deadline() {
        let stack = empty_stack();
        show_toast(&stack, "just expired");
        backdate_last(&stack, NOTICE_TOAST_DURATION + Duration::from_millis(1));
        let deadline = earliest_toast_deadline(&stack)
            .expect("an expired-but-unpruned toast still schedules a wake");
        assert!(deadline <= Instant::now(), "the deadline is in the past");
    }

    /// Raising a toast identical to a live one refreshes that toast's timer
    /// instead of stacking a duplicate (key repeat on a refused chord).
    /// Different content still stacks.
    #[test]
    fn identical_toasts_refresh_instead_of_stacking() {
        let stack = empty_stack();
        show_toast(&stack, "same message");
        backdate_last(&stack, Duration::from_millis(500));
        let first_deadline = stack.borrow()[0].deadline();

        show_toast(&stack, "same message");
        assert_eq!(stack.borrow().len(), 1, "the duplicate did not stack");
        assert!(
            stack.borrow()[0].deadline() > first_deadline,
            "the duplicate refreshed the live toast's timer"
        );

        show_toast(&stack, "different message");
        assert_eq!(stack.borrow().len(), 2, "different content stacks");
    }

    /// Every busy refusal shares the same two-row shape: the message row and
    /// the remedy row, resolved from the keybinding data.
    #[test]
    fn busy_refusal_carries_the_remedy_row() {
        let body = busy_refusal("switch sessions");
        assert_eq!(
            body.rows.iter().map(|r| row_text(r)).collect::<Vec<_>>(),
            vec![
                "Can't switch sessions while work is running".to_string(),
                "Ctrl+C cancels \u{00b7} Alt+A stops tasks".to_string(),
            ]
        );
        let picker = action_shortcut(ACTION_AGENT_PICKER).expect("picker chord bound");
        for what in ["switch branches", "branch", "start a new session"] {
            let body = busy_refusal(what);
            assert_eq!(body.rows.len(), 2, "two rows for {what}");
            assert_eq!(
                row_text(&body.rows[0]),
                format!("Can't {what} while work is running")
            );
            let remedy = row_text(&body.rows[1]);
            assert!(remedy.contains(fixed_keys::CTRL_C), "{remedy}");
            assert!(remedy.contains(&picker), "{remedy}");
        }
    }

    /// Every busy refusal draws on a classic 80-column terminal: the two-row
    /// split keeps each row narrow enough for the frame to fit.
    #[test]
    fn busy_refusals_fit_an_80_column_terminal() {
        for what in [
            "switch sessions",
            "switch branches",
            "branch",
            "start a new session",
        ] {
            let stack = empty_stack();
            show_toast(&stack, busy_refusal(what));
            let toasts = widget_over(&stack);
            let ctx = draw_ctx(80, Some(24));
            let surfs = toasts.draw_stack(
                &ctx,
                Size {
                    width: 80,
                    height: 24,
                },
            );
            assert_eq!(surfs.len(), 1, "the {what} refusal fits 80 columns");
            let body = rows(&surfs[0]).join("\n");
            assert!(
                body.contains(&format!("Can't {what} while work is running")),
                "{body:?}"
            );
            assert!(body.contains("stops tasks"), "{body:?}");
        }
    }
}
