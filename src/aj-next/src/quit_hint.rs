//! The Ctrl+C quit-arm hint: a small floating box shown while the quit
//! sequence is armed (the first Ctrl+C landed, the second is pending).
//!
//! It replaces the inline transcript notice with a corner popup anchored
//! above the editor, spelling out the ladder: press Ctrl+C again to quit,
//! or Esc to cancel. When a quit would tear down background agents or tasks,
//! a dim warning row above the ladder names the running work, so the user
//! knows what quitting would kill (the one piece of information the bare
//! ladder can't convey).
//!
//! The box is drawn straight from the live keymap state by the Shell, so it
//! appears and clears with the armed state. Only the running-work warning is
//! host-provided (the widgets can't reach the task registry), refreshed by
//! the drive loop on the arming edge.

use std::cell::RefCell;
use std::rc::Rc;

use aj_app::keybindings::fixed_keys;
use vaxis::cell::Style;
use vaxis::vxfw::{
    DrawContext, MaxSize, OVERLAY_WINDOW_CHROME_COLS, OVERLAY_WINDOW_CHROME_ROWS, Overflow,
    OverlayWindow, RichText, Size, Surface, TextAlign, TextSpan, Widget, WidgetRef, WidthBasis,
};

use crate::overlay::{OverlayChrome, close_key_label};
use crate::transcript::TranscriptStyles;

/// The primary action word of the quit rung. Lower-case to match the cancel
/// rung and the app's other key hints (`for commands`, `to close`).
const QUIT_LABEL: &str = "quit";
/// The action word of the cancel (disarm) rung.
const CANCEL_LABEL: &str = "cancel";
/// One column between the key column and its action word.
const KEY_LABEL_GAP: usize = 1;

/// The quit-arm hint box.
///
/// Styles come from two shared sources so a runtime theme swap re-tints the
/// box without rebuilding it: `styles` (the body's key and label colors,
/// pushed by [`set_styles`](QuitHint::set_styles)) and `chrome` (the frame
/// border and title, read live from the cell the Shell also restyles).
/// `warning` is the running-work summary the drive loop refreshes on the
/// arming edge, `None` when nothing runs.
pub(crate) struct QuitHint {
    styles: Rc<TranscriptStyles>,
    chrome: Rc<RefCell<OverlayChrome>>,
    warning: Rc<RefCell<Option<String>>>,
}

impl QuitHint {
    pub(crate) fn new(
        styles: Rc<TranscriptStyles>,
        chrome: Rc<RefCell<OverlayChrome>>,
        warning: Rc<RefCell<Option<String>>>,
    ) -> QuitHint {
        QuitHint {
            styles,
            chrome,
            warning,
        }
    }

    /// Replace the body styles, for a runtime theme swap. The frame styles
    /// live in the shared `chrome` cell and need no push here.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        self.styles = styles;
    }

    /// Build the hint box surface at its natural size, or `None` when it
    /// doesn't fit within `avail` (a too-short or too-narrow terminal). The
    /// caller anchors the returned surface. `ctx` supplies width measurement
    /// and the child's draw constraints.
    pub(crate) fn draw(&self, ctx: &DrawContext, avail: Size) -> Option<Surface> {
        let quit_key = fixed_keys::CTRL_C;
        let cancel_key = close_key_label();
        let title = format!("{quit_key} then");
        let warning = self.warning.borrow().clone();

        // Right-align the two keys into a common column so their action words
        // start at the same offset.
        let key_col = ctx
            .string_width(quit_key)
            .max(ctx.string_width(&cancel_key));
        let pad = |key: &str| " ".repeat(key_col.saturating_sub(ctx.string_width(key)));

        let span = |text: String, style: Style| TextSpan {
            text,
            style,
            ..TextSpan::default()
        };
        let mut spans = Vec::new();
        if let Some(w) = &warning {
            spans.push(span(format!("{w}\n"), self.styles.dim));
        }
        // Each rung is `<pad><key> <label>`, styled like the splash's
        // `ctrl+o for commands` hint: the key in the keybinding-hint style,
        // its action word dimmed. The non-final rung carries the hard line
        // break.
        spans.push(span(pad(quit_key), self.styles.dim));
        spans.push(span(quit_key.to_string(), self.styles.keybinding_hint));
        spans.push(span(format!(" {QUIT_LABEL}\n"), self.styles.dim));
        spans.push(span(pad(&cancel_key), self.styles.dim));
        spans.push(span(cancel_key, self.styles.keybinding_hint));
        spans.push(span(format!(" {CANCEL_LABEL}"), self.styles.dim));

        // Interior content extent. The widest rung, or the warning if longer,
        // sets the width; the ladder plus an optional warning set the height.
        let content_width = (key_col + KEY_LABEL_GAP + ctx.string_width(QUIT_LABEL))
            .max(key_col + KEY_LABEL_GAP + ctx.string_width(CANCEL_LABEL))
            .max(warning.as_ref().map_or(0, |w| ctx.string_width(w)));
        let content_rows = 2 + usize::from(warning.is_some());

        // The frame adds chrome on every side, and the top edge must be wide
        // enough to inline the title (`OverlayWindow` insets it two columns and
        // pads it with a space on each side).
        let chrome_cols = usize::from(OVERLAY_WINDOW_CHROME_COLS);
        let title_min_width = ctx.string_width(&title) + chrome_cols + 2;
        let box_width = (content_width + chrome_cols).max(title_min_width);
        let box_height = content_rows + usize::from(OVERLAY_WINDOW_CHROME_ROWS);

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
            // No soft wrap: the interior is sized to the content, so lines
            // never wrap, and the ellipsis overflow is a belt-and-braces guard
            // for a pathological width clamp.
            softwrap: false,
            overflow: Overflow::Ellipsis,
            width_basis: WidthBasis::LongestLine,
        }));
        let chrome = self.chrome.borrow();
        let mut win = OverlayWindow::new(title, child);
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
    use crate::test_support::{draw_ctx, flatten, rows};

    fn theme() -> Theme {
        Theme::bundled_dark_with_mode(ColorMode::Truecolor)
    }

    fn hint(warning: Option<&str>) -> QuitHint {
        let t = theme();
        QuitHint::new(
            Rc::new(TranscriptStyles::from_theme(&t)),
            Rc::new(RefCell::new(OverlayChrome::from_theme(&t))),
            Rc::new(RefCell::new(warning.map(str::to_string))),
        )
    }

    /// Roomy available area: the box always fits.
    fn roomy() -> Size {
        Size {
            width: 200,
            height: 50,
        }
    }

    #[test]
    fn draws_the_ladder_with_the_title_and_no_warning() {
        let hint = hint(None);
        let surf = hint
            .draw(&draw_ctx(200, Some(50)), roomy())
            .expect("box fits");
        let r = rows(&surf);
        // Frame plus a two-row ladder: no warning row.
        assert_eq!(surf.size.height, 6, "{r:?}");
        assert!(r[0].contains("Ctrl+C then"), "title on top edge: {r:?}");
        assert!(
            r[2].contains(&format!("{} quit", fixed_keys::CTRL_C)),
            "{r:?}"
        );
        assert!(
            r[3].contains(&format!("{} cancel", close_key_label())),
            "{r:?}"
        );
    }

    #[test]
    fn prepends_the_running_work_warning() {
        let hint = hint(Some("2 agents / 1 task still running"));
        let surf = hint
            .draw(&draw_ctx(200, Some(50)), roomy())
            .expect("box fits");
        let r = rows(&surf);
        // The warning takes a third content row above the ladder.
        assert_eq!(surf.size.height, 7, "{r:?}");
        assert!(r[2].contains("2 agents / 1 task still running"), "{r:?}");
        assert!(
            r[3].contains(&format!("{} quit", fixed_keys::CTRL_C)),
            "{r:?}"
        );
        assert!(
            r[4].contains(&format!("{} cancel", close_key_label())),
            "{r:?}"
        );
    }

    #[test]
    fn keys_use_the_keybinding_hint_style_and_labels_are_dim() {
        let t = theme();
        let styles = TranscriptStyles::from_theme(&t);
        let hint = QuitHint::new(
            Rc::new(TranscriptStyles::from_theme(&t)),
            Rc::new(RefCell::new(OverlayChrome::from_theme(&t))),
            Rc::new(RefCell::new(None)),
        );
        let surf = hint
            .draw(&draw_ctx(200, Some(50)), roomy())
            .expect("box fits");
        let grid = flatten(&surf);
        // The quit rung sits at content row 2. Row 0 is the titled border,
        // which also carries the key text, so we scan the rung, not the box.
        let rung = &grid[2];
        // The `+` occurs only in "Ctrl+C": the key uses the keybinding-hint
        // style, matching the splash's `ctrl+o` hint.
        let key = rung
            .iter()
            .find(|c| c.char.grapheme() == "+")
            .expect("the key's '+'");
        assert_eq!(key.style, styles.keybinding_hint);
        // The `q` occurs only in "quit": the action word is dimmed.
        let label = rung
            .iter()
            .find(|c| c.char.grapheme() == "q")
            .expect("the label's 'q'");
        assert_eq!(label.style, styles.dim);
    }

    #[test]
    fn declines_when_it_does_not_fit() {
        let hint = hint(None);
        let ctx = draw_ctx(200, Some(50));
        // Too narrow (the box needs the title width plus chrome).
        assert!(
            hint.draw(
                &ctx,
                Size {
                    width: 8,
                    height: 50
                }
            )
            .is_none()
        );
        // Too short for the frame plus the ladder.
        assert!(
            hint.draw(
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
