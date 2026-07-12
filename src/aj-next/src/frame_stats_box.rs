//! The frame-statistics debug overlay: a small, opt-in corner box that
//! surfaces render-loop health (last / avg / max frame time, redraw rate,
//! changed-cell count, and screen size).
//!
//! It reads a host-refreshed [`FrameStats`] snapshot the drive loop writes
//! just before each paint, so the box shows the previous frame's numbers and
//! freezes when the UI is idle (no new frame is produced). It is
//! non-interactive: built straight from `OverlayWindow`/`RichText` whose
//! surfaces carry no widget identity, so it never joins the focus path and,
//! occupying only its own cells, leaves hit-testing outside it untouched.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use vaxis::cell::Style;
use vaxis::vxfw::{
    DrawContext, FrameStats, MaxSize, OVERLAY_WINDOW_CHROME_COLS, OVERLAY_WINDOW_CHROME_ROWS,
    Overflow, OverlayWindow, RichText, Size, Surface, TextAlign, TextSpan, Widget, WidgetRef,
    WidthBasis,
};

use crate::overlay::OverlayChrome;
use crate::transcript::TranscriptStyles;

/// The box title, shown inline on its top edge.
const TITLE: &str = "frame stats";
/// One column between a label and its value.
const LABEL_VALUE_GAP: usize = 1;
/// Placeholder shown before the first frame produces a snapshot.
const EMPTY_LABEL: &str = "collecting\u{2026}";

/// The frame-statistics debug overlay box.
///
/// Styles come from two shared sources so a runtime theme swap re-tints the
/// box without rebuilding it: `styles` (the body's label and value colors)
/// and `chrome` (the frame border and title, read live from the cell the
/// Shell also restyles). `stats` is the latest snapshot the drive loop
/// refreshes before each paint, `None` before the first frame.
pub(crate) struct FrameStatsBox {
    styles: Rc<TranscriptStyles>,
    chrome: Rc<RefCell<OverlayChrome>>,
    stats: Rc<Cell<Option<FrameStats>>>,
}

impl FrameStatsBox {
    pub(crate) fn new(
        styles: Rc<TranscriptStyles>,
        chrome: Rc<RefCell<OverlayChrome>>,
        stats: Rc<Cell<Option<FrameStats>>>,
    ) -> FrameStatsBox {
        FrameStatsBox {
            styles,
            chrome,
            stats,
        }
    }

    /// Replace the body styles, for a runtime theme swap. The frame styles
    /// live in the shared `chrome` cell and need no push here.
    pub(crate) fn set_styles(&mut self, styles: Rc<TranscriptStyles>) {
        self.styles = styles;
    }

    /// Build the box surface at its natural size, or `None` when it doesn't
    /// fit within `avail` (a too-short or too-narrow terminal). The caller
    /// anchors the returned surface. `ctx` supplies width measurement and the
    /// child's draw constraints.
    pub(crate) fn draw(&self, ctx: &DrawContext, avail: Size) -> Option<Surface> {
        // Before the first frame we have no snapshot, so show a single dim
        // placeholder rather than a wall of zeros.
        let rows: Vec<(String, String)> = match self.stats.get() {
            Some(stats) => stat_rows(&stats),
            None => vec![(EMPTY_LABEL.to_string(), String::new())],
        };

        // Left-align the labels into a common column so the values start at
        // the same offset.
        let label_col = rows
            .iter()
            .map(|(l, _)| ctx.string_width(l))
            .max()
            .unwrap_or(0);

        let span = |text: String, style: Style| TextSpan {
            text,
            style,
            ..TextSpan::default()
        };
        let mut spans = Vec::new();
        for (i, (label, value)) in rows.iter().enumerate() {
            let newline = if i + 1 < rows.len() { "\n" } else { "" };
            if value.is_empty() {
                spans.push(span(format!("{label}{newline}"), self.styles.dim));
                continue;
            }
            // `<label><pad> <value>`: the label dimmed, the value in the accent
            // key-hint style, matching the quit hint's key/label split.
            let pad = " ".repeat(label_col.saturating_sub(ctx.string_width(label)));
            spans.push(span(format!("{label}{pad} "), self.styles.dim));
            spans.push(span(
                format!("{value}{newline}"),
                self.styles.keybinding_hint,
            ));
        }

        // Interior extent: the widest row sets the width, one row per stat sets
        // the height. A value-less placeholder row is just its label.
        let content_width = rows
            .iter()
            .map(|(l, v)| {
                if v.is_empty() {
                    ctx.string_width(l)
                } else {
                    label_col + LABEL_VALUE_GAP + ctx.string_width(v)
                }
            })
            .max()
            .unwrap_or(0);
        let content_rows = rows.len();

        // The frame adds chrome on every side, and the top edge must be wide
        // enough to inline the title (`OverlayWindow` insets it two columns and
        // pads it with a space on each side).
        let chrome_cols = usize::from(OVERLAY_WINDOW_CHROME_COLS);
        let title_min_width = ctx.string_width(TITLE) + chrome_cols + 2;
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
        let mut win = OverlayWindow::new(TITLE.to_string(), child);
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

/// The `(label, value)` rows for a snapshot, in display order.
fn stat_rows(stats: &FrameStats) -> Vec<(String, String)> {
    vec![
        ("last".to_string(), fmt_ms(stats.last)),
        ("avg".to_string(), fmt_ms(stats.avg)),
        ("max".to_string(), fmt_ms(stats.max)),
        ("fps".to_string(), format!("{:.0}", stats.fps)),
        ("cells".to_string(), stats.last_cells.to_string()),
        // `FrameStats.size` is `(rows, cols)`; the box prints `cols x rows`.
        (
            "size".to_string(),
            format!("{}x{}", stats.size.1, stats.size.0),
        ),
    ]
}

/// Format a render time as milliseconds with one decimal, e.g. `1.2ms`.
/// Sub-millisecond spans show as `0.3ms`.
fn fmt_ms(d: Duration) -> String {
    format!("{:.1}ms", d.as_secs_f64() * 1000.0)
}

#[cfg(test)]
mod tests {
    use aj_app::theme::{ColorMode, Theme};

    use super::*;
    use crate::test_support::{draw_ctx, rows};

    fn theme() -> Theme {
        Theme::bundled_dark_with_mode(ColorMode::Truecolor)
    }

    fn stats() -> FrameStats {
        FrameStats {
            last: Duration::from_micros(1200),
            avg: Duration::from_micros(900),
            max: Duration::from_micros(3400),
            frames: 120,
            fps: 62.4,
            last_cells: 1234,
            // (rows, cols): the box must print `cols x rows`.
            size: (30, 100),
        }
    }

    fn box_with(stats: Option<FrameStats>) -> FrameStatsBox {
        let t = theme();
        FrameStatsBox::new(
            Rc::new(TranscriptStyles::from_theme(&t)),
            Rc::new(RefCell::new(OverlayChrome::from_theme(&t))),
            Rc::new(Cell::new(stats)),
        )
    }

    fn roomy() -> Size {
        Size {
            width: 200,
            height: 50,
        }
    }

    /// `fmt_ms` renders one decimal and keeps sub-millisecond spans visible.
    #[test]
    fn fmt_ms_uses_one_decimal() {
        assert_eq!(fmt_ms(Duration::from_micros(1200)), "1.2ms");
        assert_eq!(fmt_ms(Duration::from_micros(300)), "0.3ms");
        assert_eq!(fmt_ms(Duration::from_millis(3)), "3.0ms");
    }

    /// A known snapshot renders the expected label/value lines, pinning the ms
    /// formatting and the `cols x rows` size ordering.
    #[test]
    fn renders_the_expected_lines() {
        let b = box_with(Some(stats()));
        let surf = b.draw(&draw_ctx(200, Some(50)), roomy()).expect("box fits");
        let r = rows(&surf);
        let body = r.join("\n");
        assert!(body.contains(TITLE), "title on top edge: {r:?}");
        assert!(body.contains("last  1.2ms"), "{r:?}");
        assert!(body.contains("avg   0.9ms"), "{r:?}");
        assert!(body.contains("max   3.4ms"), "{r:?}");
        assert!(body.contains("fps   62"), "{r:?}");
        // fps is a whole number: no other row introduces "62.", so this pins
        // the `{:.0}` format against a decimal-precision regression.
        assert!(!body.contains("62."), "fps renders without decimals: {r:?}");
        assert!(body.contains("cells 1234"), "{r:?}");
        // (rows, cols) = (30, 100) prints as cols x rows.
        assert!(body.contains("size  100x30"), "{r:?}");
    }

    /// The box's own surface carries no widget identity, so it never joins the
    /// focus path.
    #[test]
    fn box_surface_is_non_interactive() {
        let b = box_with(Some(stats()));
        let surf = b.draw(&draw_ctx(200, Some(50)), roomy()).expect("box fits");
        assert!(surf.widget.is_none(), "the box must be non-interactive");
    }

    /// Before the first frame the box shows a dim placeholder rather than
    /// zeros.
    #[test]
    fn empty_state_shows_a_placeholder() {
        let b = box_with(None);
        let surf = b.draw(&draw_ctx(200, Some(50)), roomy()).expect("box fits");
        let body = rows(&surf).join("\n");
        assert!(body.contains(EMPTY_LABEL), "{body:?}");
    }

    /// It declines when the terminal can't fit the frame plus content.
    #[test]
    fn declines_when_it_does_not_fit() {
        let b = box_with(Some(stats()));
        let ctx = draw_ctx(200, Some(50));
        assert!(
            b.draw(
                &ctx,
                Size {
                    width: 4,
                    height: 50
                }
            )
            .is_none()
        );
        assert!(
            b.draw(
                &ctx,
                Size {
                    width: 200,
                    height: 2
                }
            )
            .is_none()
        );
    }
}
