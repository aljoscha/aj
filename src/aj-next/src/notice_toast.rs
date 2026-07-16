//! A transient "notice" toast: a small, non-interactive corner box that shows
//! a short text message bottom-right and clears itself after a couple of
//! seconds.
//!
//! It is the copied-to-clipboard toast's plainer sibling: it reads a
//! host-shared [`Notice`] record (the message plus when it was raised), frames
//! it with the shared [`corner_box`](crate::corner_box::corner_box), and so is
//! non-interactive and never joins the focus path. It reuses the copy toast's
//! [`COPIED_TOAST_DURATION`] so the two boxes feel of a piece.
//!
//! Like the copy toast it has no self-timer. The drive loop wakes at the
//! record's deadline and requests a repaint, and this `draw` returns `None`
//! once the record has expired, so the box clears itself on that repaint.
//! Raising a fresh notice overwrites the record, which moves the deadline
//! forward and so resets the timer.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use vaxis::vxfw::{DrawContext, Size, Surface};

use crate::copied_toast::COPIED_TOAST_DURATION;
use crate::corner_box::{CornerBoxBody, corner_box, span};
use crate::overlay::OverlayChrome;
use crate::transcript::TranscriptStyles;

/// A raised notice: the message to show and when it was raised. The host
/// writes it (see [`raise_toast`]); the toast (and the drive loop's wake
/// scheduling) read it.
#[derive(Clone)]
pub(crate) struct Notice {
    pub(crate) message: String,
    pub(crate) at: Instant,
}

impl Notice {
    /// Whether this record is still within the toast's display window.
    pub(crate) fn is_live(&self) -> bool {
        self.at.elapsed() < COPIED_TOAST_DURATION
    }
}

/// Raise a transient notice into `cell`, replacing any live one and resetting
/// its timer. The caller still owns the repaint: raise this, then request a
/// redraw so the box appears immediately (the drive loop schedules the
/// clearing repaint at its deadline).
pub(crate) fn raise_toast(cell: &Rc<RefCell<Option<Notice>>>, message: impl Into<String>) {
    *cell.borrow_mut() = Some(Notice {
        message: message.into(),
        at: Instant::now(),
    });
}

/// The transient notice toast box.
///
/// Styles come from two shared sources so a runtime theme swap re-tints the
/// box without rebuilding it: `styles` (the body color) and `chrome` (the
/// frame border, read live from the cell the Shell also restyles). `notice`
/// is the latest raised notice, shared with the host that writes it, `None`
/// before the first toast and after one expires.
pub(crate) struct NoticeToast {
    styles: Rc<TranscriptStyles>,
    chrome: Rc<RefCell<OverlayChrome>>,
    notice: Rc<RefCell<Option<Notice>>>,
}

impl NoticeToast {
    pub(crate) fn new(
        styles: Rc<TranscriptStyles>,
        chrome: Rc<RefCell<OverlayChrome>>,
        notice: Rc<RefCell<Option<Notice>>>,
    ) -> NoticeToast {
        NoticeToast {
            styles,
            chrome,
            notice,
        }
    }

    /// Replace the body style, for a runtime theme swap. The frame styles live
    /// in the shared `chrome` cell and need no push here.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        self.styles = styles;
    }

    /// Build the toast surface at its natural size, or `None` when there is no
    /// live notice to report or the box does not fit within `avail`. The
    /// caller anchors the returned surface. `ctx` supplies width measurement
    /// and the child's draw constraints.
    pub(crate) fn draw(&self, ctx: &DrawContext, avail: Size) -> Option<Surface> {
        let notice = self.notice.borrow();
        let notice = notice.as_ref().filter(|n| n.is_live())?;
        let content_width = ctx.string_width(&notice.message);
        let spans = vec![span(notice.message.clone(), self.styles.dim)];
        corner_box(
            ctx,
            &self.chrome.borrow(),
            avail,
            CornerBoxBody {
                title: String::new(),
                spans,
                content_width,
                content_rows: 1,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use aj_app::theme::{ColorMode, Theme};

    use super::*;
    use crate::test_support::{draw_ctx, rows};

    fn theme() -> Theme {
        Theme::bundled_dark_with_mode(ColorMode::Truecolor)
    }

    fn toast_with(notice: Option<Notice>) -> NoticeToast {
        let t = theme();
        NoticeToast::new(
            Rc::new(TranscriptStyles::from_theme(&t)),
            Rc::new(RefCell::new(OverlayChrome::from_theme(&t))),
            Rc::new(RefCell::new(notice)),
        )
    }

    fn roomy() -> Size {
        Size {
            width: 200,
            height: 50,
        }
    }

    /// With no notice on record the toast draws nothing.
    #[test]
    fn no_notice_draws_nothing() {
        let toast = toast_with(None);
        assert!(toast.draw(&draw_ctx(200, Some(50)), roomy()).is_none());
    }

    /// An expired notice draws nothing, so the box clears itself on the
    /// repaint the drive loop schedules at the deadline.
    #[test]
    fn expired_notice_draws_nothing() {
        let stale = Notice {
            message: "gone".to_string(),
            at: Instant::now() - COPIED_TOAST_DURATION - Duration::from_millis(1),
        };
        let toast = toast_with(Some(stale));
        assert!(toast.draw(&draw_ctx(200, Some(50)), roomy()).is_none());
    }

    /// A live notice renders its message.
    #[test]
    fn live_notice_reports_the_message() {
        let toast = toast_with(Some(Notice {
            message: "Can't switch sessions while work is running.".to_string(),
            at: Instant::now(),
        }));
        let surf = toast
            .draw(&draw_ctx(200, Some(50)), roomy())
            .expect("box fits");
        let body = rows(&surf).join("\n");
        assert!(
            body.contains("Can't switch sessions while work is running."),
            "{body:?}"
        );
    }

    /// `raise_toast` writes a live record into the shared cell.
    #[test]
    fn raise_toast_writes_a_live_record() {
        let cell: Rc<RefCell<Option<Notice>>> = Rc::new(RefCell::new(None));
        raise_toast(&cell, "hello");
        let borrowed = cell.borrow();
        let notice = borrowed.as_ref().expect("a notice was raised");
        assert_eq!(notice.message, "hello");
        assert!(notice.is_live(), "a freshly raised notice is live");
    }

    /// The box's own surface carries no widget identity, so it never joins the
    /// focus path.
    #[test]
    fn box_surface_is_non_interactive() {
        let toast = toast_with(Some(Notice {
            message: "hi".to_string(),
            at: Instant::now(),
        }));
        let surf = toast
            .draw(&draw_ctx(200, Some(50)), roomy())
            .expect("box fits");
        assert!(surf.widget.is_none(), "the box must be non-interactive");
    }
}
