//! The generic transcript bubble: a full-width tinted rectangle
//! around wrapped rich text.
//!
//! Tool cells and user-message bubbles share the exact same surface
//! mechanics (one-column inset, one bg-painted blank row above and
//! below, an untinted trailing spacer row), so the widget is generic
//! over the flattened span content and the tint. The builders in
//! `tool_cell` and `transcript` decide what goes inside.

use vaxis::cell::{Cell, Color, Style};
use vaxis::vxfw::{
    DrawContext, MaxSize, RelativePoint, RichText, Size, SubSurface, Surface, TextSpan, Widget,
    WidthBasis,
};

/// Horizontal padding inside the bubble (one column on each side so
/// the tinted rectangle reads as an inset block rather than
/// edge-to-edge text).
pub(crate) const PADDING_X: u16 = 1;
/// Vertical padding inside the bubble: one bg-painted blank row above
/// and below the content.
pub(crate) const PADDING_Y: u16 = 1;

/// Minimum total render width at which the bubble framing kicks in.
/// Below this we fall back to a plain listing so the bg-padding
/// pipeline (two cells of horizontal padding plus at least one cell
/// of content) doesn't paint a degenerate row.
pub(crate) const MIN_BUBBLE_WIDTH: u16 = 3;

/// A full-width tinted bubble entry in the transcript. Built fresh
/// per draw by the transcript's entry builders.
pub(crate) struct Bubble {
    /// The content, flattened with `\n` hard-break separator spans.
    pub(crate) text: Vec<TextSpan>,
    /// The bubble's background tint. `None` renders the bare wrapped
    /// text with no bubble (the `header_only` mode used inside
    /// sub-agent boxes).
    pub(crate) bg: Option<Color>,
    /// Style used for the trailing spacer row and the plain
    /// fallback paths.
    pub(crate) base: Style,
    /// When false, long content lines truncate with an ellipsis at
    /// the inner width instead of wrapping. The pending-message box
    /// uses this so a wide draft can't grow the box row by row.
    pub(crate) softwrap: bool,
    /// Whether the untinted spacer row below the bubble is drawn.
    /// Transcript entries carry it so consecutive rows don't collide;
    /// the pending-message box sits flush above the editor and skips
    /// it.
    pub(crate) trailing_spacer: bool,
}

impl Bubble {
    /// A transcript-entry bubble: wrapped content plus the trailing
    /// untinted spacer row every transcript entry carries.
    pub(crate) fn entry(text: Vec<TextSpan>, bg: Option<Color>, base: Style) -> Bubble {
        Bubble {
            text,
            bg,
            base,
            softwrap: true,
            trailing_spacer: true,
        }
    }

    /// Plain fallback: wrapped text with no bubble or background,
    /// plus the one-blank-row spacer every transcript entry carries.
    /// Used for `header_only` cells and for degenerate widths where
    /// the bubble framing can't paint.
    fn draw_plain(&self, ctx: &DrawContext) -> Surface {
        let mut spans = self.text.clone();
        // A trailing "\n\n" adds one empty hard line, which the wrap
        // engine renders as the blank spacer row (the same shape the
        // other transcript entries use).
        spans.push(TextSpan {
            text: "\n\n".into(),
            style: self.base,
            ..TextSpan::default()
        });
        let mut rich = RichText::new(spans);
        rich.softwrap = self.softwrap;
        rich.draw(ctx)
    }
}

impl Widget for Bubble {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        let width = ctx.max.width.unwrap_or(ctx.min.width);
        let Some(bg) = self.bg else {
            return self.draw_plain(ctx);
        };
        if width < MIN_BUBBLE_WIDTH {
            return self.draw_plain(ctx);
        }

        let bg_style = Style {
            bg,
            ..Style::default()
        };

        // Lay the content out at the inset width. `Parent` width
        // basis plus the bg-carrying base style make the content
        // surface span the full inner width with tinted fill cells,
        // so short lines' tails paint the tint too.
        let inner_width = width - 2 * PADDING_X;
        let inner_ctx = ctx.with_constraints(
            Size {
                width: inner_width,
                height: 0,
            },
            MaxSize {
                width: Some(inner_width),
                height: None,
            },
        );
        let mut rich = RichText::new(self.text.clone());
        rich.width_basis = WidthBasis::Parent;
        rich.base_style = bg_style;
        rich.softwrap = self.softwrap;
        let mut inner = rich.draw(&inner_ctx);
        // Span-styled cells keep their own (bg-less) style when the
        // wrap engine writes them over the base fill, so stamp the
        // tint onto every cell after the fact.
        for cell in &mut inner.buffer {
            cell.style.bg = bg;
        }

        // The outer surface: bg-filled padding frame around the
        // content, plus (for transcript entries) one default
        // (untinted) spacer row at the bottom standing in for the
        // `\n\n` spacer the span-based entries carry.
        let content_height = inner.size.height;
        let bubble_height = content_height + 2 * PADDING_Y;
        let mut surface = Surface::with_size(Size {
            width,
            height: bubble_height + u16::from(self.trailing_spacer),
        });
        let bg_cell = Cell {
            style: bg_style,
            ..Cell::default()
        };
        for row in 0..bubble_height {
            for col in 0..width {
                surface.write_cell(col, row, bg_cell.clone());
            }
        }
        surface.children.push(SubSurface {
            origin: RelativePoint {
                col: i32::from(PADDING_X),
                row: i32::from(PADDING_Y),
            },
            surface: inner,
            z_index: 0,
        });
        surface
    }
}
