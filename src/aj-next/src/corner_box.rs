//! Shared construction for the small bordered "corner boxes" the Shell floats
//! above the editor: the frame-stats overlay, the quit-arm hint, and the
//! copied-to-clipboard toast.
//!
//! Each box supplies its own title and a measured body (the styled spans plus
//! the interior extent). This module frames that body in a bordered,
//! optionally-titled `OverlayWindow` sized to its content or its title,
//! whichever is wider: it applies the chrome, enforces the title's minimum
//! width, and declines when the box would not fit. A corner box is
//! non-interactive: neither its frame nor its text wants events, and the Shell
//! never gives it focus, so it never joins the focus path and leaves
//! hit-testing outside it untouched.

use std::cell::RefCell;
use std::rc::Rc;

use vaxis::cell::Style;
use vaxis::vxfw::{
    DrawContext, MaxSize, OVERLAY_WINDOW_CHROME_COLS, OVERLAY_WINDOW_CHROME_ROWS, Overflow,
    OverlayWindow, RichText, Size, Surface, TextAlign, TextSpan, Widget, WidgetRef, WidthBasis,
};

use crate::overlay::OverlayChrome;

/// A measured corner-box body: the styled interior spans plus the interior
/// extent the caller computed from them.
///
/// `content_width` and `content_rows` are the interior size in cells, without
/// the frame chrome. The caller measures them because only it knows the
/// content's layout (label columns, key ladders, an optional warning row).
///
/// The caller must keep the extent in agreement with `spans`: they should
/// render within `content_width` by `content_rows`. Over-measuring pads the
/// box, under-measuring trips the ellipsis overflow guard.
pub(crate) struct CornerBoxBody {
    /// Inline title on the top border. Empty for an untitled box (the toast).
    pub(crate) title: String,
    pub(crate) spans: Vec<TextSpan>,
    pub(crate) content_width: usize,
    pub(crate) content_rows: usize,
}

/// A `TextSpan` with default flags, for building corner-box bodies.
pub(crate) fn span(text: String, style: Style) -> TextSpan {
    TextSpan {
        text,
        style,
        ..TextSpan::default()
    }
}

/// Frame a measured body into a bordered box at its natural size, or `None`
/// when it doesn't fit within `avail` (a too-short or too-narrow terminal).
/// The caller anchors the returned surface.
///
/// `ctx` supplies width measurement and the child's draw constraints. `chrome`
/// supplies the border and title styles, read live so a runtime theme swap
/// re-tints the box without rebuilding it.
pub(crate) fn corner_box(
    ctx: &DrawContext,
    chrome: &OverlayChrome,
    avail: Size,
    body: CornerBoxBody,
) -> Option<Surface> {
    let CornerBoxBody {
        title,
        spans,
        content_width,
        content_rows,
    } = body;

    // The frame adds chrome on every side, and a non-empty title needs the top
    // edge wide enough to inline it (`OverlayWindow` insets the title two
    // columns and pads it with a space on each side). An empty title imposes
    // no minimum beyond the chrome.
    let chrome_cols = usize::from(OVERLAY_WINDOW_CHROME_COLS);
    let title_min_width = if title.is_empty() {
        0
    } else {
        ctx.string_width(&title) + chrome_cols + 2
    };
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
        // No soft wrap: the interior is sized to the content, so lines never
        // wrap, and the ellipsis overflow is a belt-and-braces guard for a
        // pathological width clamp.
        softwrap: false,
        overflow: Overflow::Ellipsis,
        width_basis: WidthBasis::LongestLine,
    }));
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

#[cfg(test)]
mod tests {
    use aj_app::theme::{ColorMode, Theme};

    use super::*;
    use crate::test_support::{draw_ctx, rows};

    fn chrome() -> OverlayChrome {
        OverlayChrome::from_theme(&Theme::bundled_dark_with_mode(ColorMode::Truecolor))
    }

    /// Roomy available area: the box always fits.
    fn roomy() -> Size {
        Size {
            width: 200,
            height: 50,
        }
    }

    fn chrome_cols() -> usize {
        usize::from(OVERLAY_WINDOW_CHROME_COLS)
    }

    /// A body whose rendered child fills exactly `content_width` by
    /// `content_rows`, so the drawn size matches the declared extent (the real
    /// widgets keep the two in sync).
    fn body(title: &str, content_width: usize, content_rows: usize) -> CornerBoxBody {
        let line = "x".repeat(content_width);
        let text = vec![line; content_rows].join("\n");
        CornerBoxBody {
            title: title.to_string(),
            spans: vec![span(text, Style::default())],
            content_width,
            content_rows,
        }
    }

    /// A title wider than the content forces the top edge wide enough to inline
    /// it, and the title renders in full.
    #[test]
    fn a_long_title_widens_a_narrow_content_box() {
        let ctx = draw_ctx(200, Some(50));
        let title = "a fairly long title";
        let surf = corner_box(&ctx, &chrome(), roomy(), body(title, 1, 1)).expect("box fits");
        // `+2` for the space `OverlayWindow` pads the inlined title with on
        // each side.
        assert_eq!(
            usize::from(surf.size.width),
            ctx.string_width(title) + chrome_cols() + 2,
        );
        assert!(
            rows(&surf)[0].contains(title),
            "title renders untruncated: {:?}",
            rows(&surf)
        );
    }

    /// An empty title imposes no minimum: the width is just the content plus
    /// chrome. A non-empty title wider than the content widens the box.
    #[test]
    fn an_empty_title_imposes_no_minimum() {
        let ctx = draw_ctx(200, Some(50));
        let untitled = corner_box(&ctx, &chrome(), roomy(), body("", 3, 1)).expect("box fits");
        assert_eq!(usize::from(untitled.size.width), 3 + chrome_cols());
        let titled =
            corner_box(&ctx, &chrome(), roomy(), body("a wide title", 3, 1)).expect("box fits");
        assert!(
            titled.size.width > untitled.size.width,
            "a title widens the box past its content",
        );
    }

    /// When the content is wider than the title minimum, the content sets the
    /// width and the title minimum is inert.
    #[test]
    fn content_wider_than_the_title_ignores_the_minimum() {
        let ctx = draw_ctx(200, Some(50));
        let surf = corner_box(&ctx, &chrome(), roomy(), body("hi", 40, 1)).expect("box fits");
        assert_eq!(usize::from(surf.size.width), 40 + chrome_cols());
    }

    /// The fit guard is inclusive: an `avail` exactly the box size is accepted,
    /// one column narrower or one row shorter is declined.
    #[test]
    fn the_fit_guard_is_inclusive_at_the_boundary() {
        let ctx = draw_ctx(200, Some(50));
        let natural = corner_box(&ctx, &chrome(), roomy(), body("frame stats", 20, 3))
            .expect("box fits in a roomy area")
            .size;

        assert!(
            corner_box(&ctx, &chrome(), natural, body("frame stats", 20, 3)).is_some(),
            "an exact-fit area is accepted",
        );
        assert!(
            corner_box(
                &ctx,
                &chrome(),
                Size {
                    width: natural.width - 1,
                    height: natural.height,
                },
                body("frame stats", 20, 3),
            )
            .is_none(),
            "one column too narrow is declined",
        );
        assert!(
            corner_box(
                &ctx,
                &chrome(),
                Size {
                    width: natural.width,
                    height: natural.height - 1,
                },
                body("frame stats", 20, 3),
            )
            .is_none(),
            "one row too short is declined",
        );
    }

    /// The box's own surface carries no widget identity, so it never joins the
    /// focus path.
    #[test]
    fn the_box_surface_is_non_interactive() {
        let ctx = draw_ctx(200, Some(50));
        let surf = corner_box(&ctx, &chrome(), roomy(), body("t", 5, 1)).expect("box fits");
        assert!(surf.widget.is_none(), "the box must be non-interactive");
    }
}
