//! The "copied to clipboard" toast: a small, transient box that reports how
//! many characters a mouse text selection just copied.
//!
//! It reads a host-shared [`Copied`] record the transcript writes when a
//! select-to-copy lands, and shows for [`COPIED_TOAST_DURATION`] after that.
//! Like the frame-stats box it is non-interactive, built straight from
//! `OverlayWindow`/`RichText` whose surfaces carry no widget identity, so it
//! never joins the focus path and leaves hit-testing outside it untouched.
//!
//! The box has no self-timer. The drive loop wakes at the record's deadline
//! and requests a repaint, and this `draw` returns `None` once the record has
//! expired, so the box clears itself on that repaint. A fresh copy overwrites
//! the record, which moves the deadline forward and so resets the timer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use vaxis::cell::Style;
use vaxis::vxfw::{
    DrawContext, MaxSize, OVERLAY_WINDOW_CHROME_COLS, OVERLAY_WINDOW_CHROME_ROWS, Overflow,
    OverlayWindow, RichText, Size, Surface, TextAlign, TextSpan, Widget, WidgetRef, WidthBasis,
};

use crate::overlay::OverlayChrome;
use crate::transcript::TranscriptStyles;

/// How long the toast stays up after a copy. A couple of seconds, matching the
/// quit-arm hint's timeout so the two boxes feel of a piece.
pub(crate) const COPIED_TOAST_DURATION: Duration = Duration::from_millis(2000);

/// A record of the last select-to-copy: how many characters were copied, and
/// when. The transcript writes it, the toast (and the drive loop's wake
/// scheduling) read it.
#[derive(Clone, Copy)]
pub(crate) struct Copied {
    pub(crate) chars: usize,
    pub(crate) at: Instant,
}

impl Copied {
    /// Whether this record is still within the toast's display window.
    pub(crate) fn is_live(&self) -> bool {
        self.at.elapsed() < COPIED_TOAST_DURATION
    }
}

/// The "copied to clipboard" toast box.
///
/// Styles come from two shared sources so a runtime theme swap re-tints the
/// box without rebuilding it: `styles` (the body colors) and `chrome` (the
/// frame border and title, read live from the cell the Shell also restyles).
/// `copied` is the latest select-to-copy record, shared with the transcript
/// that writes it, `None` before the first copy.
pub(crate) struct CopiedToast {
    styles: Rc<TranscriptStyles>,
    chrome: Rc<RefCell<OverlayChrome>>,
    copied: Rc<Cell<Option<Copied>>>,
}

impl CopiedToast {
    pub(crate) fn new(
        styles: Rc<TranscriptStyles>,
        chrome: Rc<RefCell<OverlayChrome>>,
        copied: Rc<Cell<Option<Copied>>>,
    ) -> CopiedToast {
        CopiedToast {
            styles,
            chrome,
            copied,
        }
    }

    /// Replace the body styles, for a runtime theme swap. The frame styles
    /// live in the shared `chrome` cell and need no push here.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        self.styles = styles;
    }

    /// Build the toast surface at its natural size, or `None` when there is no
    /// live copy to report or the box does not fit within `avail`. The caller
    /// anchors the returned surface. `ctx` supplies width measurement and the
    /// child's draw constraints.
    pub(crate) fn draw(&self, ctx: &DrawContext, avail: Size) -> Option<Surface> {
        let copied = self.copied.get().filter(Copied::is_live)?;

        // The count in the accent key-hint style, the rest dimmed, matching the
        // value/label split of the frame-stats and quit-hint boxes.
        let count = copied.chars.to_string();
        let noun = if copied.chars == 1 {
            "character"
        } else {
            "characters"
        };
        let tail = format!(" {noun} copied to clipboard");
        let span = |text: String, style: Style| TextSpan {
            text,
            style,
            ..TextSpan::default()
        };
        let spans = vec![
            span(count.clone(), self.styles.keybinding_hint),
            span(tail.clone(), self.styles.dim),
        ];

        let content_width = ctx.string_width(&count) + ctx.string_width(&tail);
        let chrome_cols = usize::from(OVERLAY_WINDOW_CHROME_COLS);
        let box_width = content_width + chrome_cols;
        let box_height = 1 + usize::from(OVERLAY_WINDOW_CHROME_ROWS);

        let size = Size {
            width: u16::try_from(box_width).ok()?,
            height: u16::try_from(box_height).ok()?,
        };
        if size.width > avail.width || size.height > avail.height {
            return None;
        }

        let child: WidgetRef = Rc::new(RefCell::new(RichText {
            text: spans,
            text_align: TextAlign::Left,
            base_style: Style::default(),
            // No soft wrap: the interior is sized to the content, so the line
            // never wraps, and the ellipsis overflow is a belt-and-braces guard
            // for a pathological width clamp.
            softwrap: false,
            overflow: Overflow::Ellipsis,
            width_basis: WidthBasis::LongestLine,
        }));
        let chrome = self.chrome.borrow();
        let mut win = OverlayWindow::new(String::new(), child);
        win.border_style = chrome.border;
        win.title_style = chrome.title;
        let win_ctx = ctx.with_constraints(
            Size {
                width: 0,
                height: 0,
            },
            MaxSize::from_size(size),
        );
        Some(win.draw(&win_ctx))
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

    fn toast_with(copied: Option<Copied>) -> CopiedToast {
        let t = theme();
        CopiedToast::new(
            Rc::new(TranscriptStyles::from_theme(&t)),
            Rc::new(RefCell::new(OverlayChrome::from_theme(&t))),
            Rc::new(Cell::new(copied)),
        )
    }

    fn roomy() -> Size {
        Size {
            width: 200,
            height: 50,
        }
    }

    /// With no copy on record the toast draws nothing.
    #[test]
    fn no_copy_draws_nothing() {
        let toast = toast_with(None);
        assert!(toast.draw(&draw_ctx(200, Some(50)), roomy()).is_none());
    }

    /// An expired copy draws nothing, so the box clears itself on the repaint
    /// the drive loop schedules at the deadline.
    #[test]
    fn expired_copy_draws_nothing() {
        let stale = Copied {
            chars: 12,
            at: Instant::now() - COPIED_TOAST_DURATION - Duration::from_millis(1),
        };
        let toast = toast_with(Some(stale));
        assert!(toast.draw(&draw_ctx(200, Some(50)), roomy()).is_none());
    }

    /// A live copy renders the count and the plural noun.
    #[test]
    fn live_copy_reports_the_count() {
        let toast = toast_with(Some(Copied {
            chars: 42,
            at: Instant::now(),
        }));
        let surf = toast
            .draw(&draw_ctx(200, Some(50)), roomy())
            .expect("box fits");
        let body = rows(&surf).join("\n");
        assert!(
            body.contains("42 characters copied to clipboard"),
            "{body:?}"
        );
    }

    /// A single character uses the singular noun.
    #[test]
    fn one_character_is_singular() {
        let toast = toast_with(Some(Copied {
            chars: 1,
            at: Instant::now(),
        }));
        let surf = toast
            .draw(&draw_ctx(200, Some(50)), roomy())
            .expect("box fits");
        let body = rows(&surf).join("\n");
        assert!(body.contains("1 character copied to clipboard"), "{body:?}");
        assert!(!body.contains("characters"), "singular noun: {body:?}");
    }

    /// The box's own surface carries no widget identity, so it never joins the
    /// focus path.
    #[test]
    fn box_surface_is_non_interactive() {
        let toast = toast_with(Some(Copied {
            chars: 3,
            at: Instant::now(),
        }));
        let surf = toast
            .draw(&draw_ctx(200, Some(50)), roomy())
            .expect("box fits");
        assert!(surf.widget.is_none(), "the box must be non-interactive");
    }

    /// It declines when the terminal can't fit the frame plus content.
    #[test]
    fn declines_when_it_does_not_fit() {
        let toast = toast_with(Some(Copied {
            chars: 12345,
            at: Instant::now(),
        }));
        let ctx = draw_ctx(200, Some(50));
        assert!(
            toast
                .draw(
                    &ctx,
                    Size {
                        width: 8,
                        height: 50
                    }
                )
                .is_none()
        );
        assert!(
            toast
                .draw(
                    &ctx,
                    Size {
                        width: 200,
                        height: 3
                    }
                )
                .is_none()
        );
    }
}
