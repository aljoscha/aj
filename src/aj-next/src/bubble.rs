//! The generic transcript bubble: a full-width tinted rectangle
//! around wrapped rich text.
//!
//! Tool cells, user-message bubbles, and task-notification bubbles
//! share the exact same surface mechanics (one-column inset, one
//! bg-painted blank row above and below, an untinted trailing spacer
//! row), so the widget is generic over the flattened span content and
//! the tint. The builders in `tool_cell` and `transcript` decide what
//! goes inside.

use vaxis::cell::{Cell, Character, Color, Style};
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

/// A heavy box-drawing border painted into a bubble's existing padding
/// frame (Spec E section 2). It marks the focused user message in
/// transcript-focus mode without reflowing the transcript: the glyphs
/// overwrite the one-cell tinted frame the bubble already reserves rather
/// than wrapping the bubble in a widget that would add a frame of its own.
pub(crate) struct BubbleBorder {
    /// Border glyph color, the theme's `borderAccent`.
    pub(crate) color: Color,
    /// Copy-key hint shown on the bottom edge (`┗━ y to copy ━┛`),
    /// pre-styled by the caller so the key and the rest can differ.
    pub(crate) label: Vec<TextSpan>,
}

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
    /// The focus border, painted into the padding frame when the real
    /// (tinted) bubble path runs. `None` on every non-focused bubble.
    pub(crate) border: Option<BubbleBorder>,
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
            border: None,
        }
    }

    /// Attach the focus border painted into the padding frame. Only the real
    /// (tinted) bubble path honors it. The plain fallback ignores it.
    pub(crate) fn with_border(mut self, border: BubbleBorder) -> Bubble {
        self.border = Some(border);
        self
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
        // Overwrite the reserved padding cells with the focus border, keeping
        // the bubble tint underneath. The content sub-surface below is inset
        // by the padding, so it never covers these frame cells.
        if let Some(border) = &self.border {
            self.paint_border(&mut surface, ctx, border, width, bubble_height, bg);
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

impl Bubble {
    /// Repaint the reserved padding frame as a heavy box-drawing border in
    /// `border.color` over the bubble tint, the copy label on the bottom edge.
    ///
    /// The frame is the one-cell padding the bubble already reserves, so the
    /// border adds no rows or columns and focusing a message cannot reflow the
    /// transcript. `bubble_height` excludes the untinted `trailing_spacer`
    /// row below it, so that row stays outside the border.
    fn paint_border(
        &self,
        surface: &mut Surface,
        ctx: &DrawContext,
        border: &BubbleBorder,
        width: u16,
        bubble_height: u16,
        bg: Color,
    ) {
        let glyph = |g: &str| Cell {
            char: Character::new(g, 1),
            style: Style {
                fg: border.color,
                bg,
                ..Style::default()
            },
            ..Cell::default()
        };
        // width >= MIN_BUBBLE_WIDTH (3) and bubble_height >= 2*PADDING_Y (2),
        // so both `last` indices are in range and the corners never collide.
        let last_col = width - 1;
        let last_row = bubble_height - 1;
        surface.write_cell(0, 0, glyph("\u{250f}"));
        surface.write_cell(last_col, 0, glyph("\u{2513}"));
        for col in 1..last_col {
            surface.write_cell(col, 0, glyph("\u{2501}"));
        }
        for row in 1..last_row {
            surface.write_cell(0, row, glyph("\u{2503}"));
            surface.write_cell(last_col, row, glyph("\u{2503}"));
        }
        surface.write_cell(0, last_row, glyph("\u{2517}"));
        surface.write_cell(last_col, last_row, glyph("\u{251b}"));
        self.paint_bottom_edge(surface, ctx, border, last_col, last_row, bg);
    }

    /// Paint the bottom edge between the corners: one `━`, a space, the label,
    /// a space, then `━` fill, matching the mock `┗━ y to copy ━…┛`.
    ///
    /// When the bubble is too narrow for the label plus its chrome cells we
    /// fill the whole edge with `━`, degrading to a plain heavy frame rather
    /// than clipping the hint.
    fn paint_bottom_edge(
        &self,
        surface: &mut Surface,
        ctx: &DrawContext,
        border: &BubbleBorder,
        last_col: u16,
        last_row: u16,
        bg: Color,
    ) {
        let border_cell = |g: &str| Cell {
            char: Character::new(g, 1),
            style: Style {
                fg: border.color,
                bg,
                ..Style::default()
            },
            ..Cell::default()
        };
        let interior = usize::from(last_col.saturating_sub(1));
        let label_width: usize = border.label.iter().map(|s| ctx.string_width(&s.text)).sum();
        // Lead `━`, a space, the label, a space: `label_width + 3` cells, with
        // the rest of the interior filled with `━`.
        let fits = label_width > 0 && label_width + 3 <= interior;
        let mut col = 1u16;
        if fits {
            surface.write_cell(col, last_row, border_cell("\u{2501}"));
            col += 1;
            surface.write_cell(col, last_row, border_cell(" "));
            col += 1;
            for span in &border.label {
                for item in ctx.grapheme_iterator(&span.text) {
                    if col >= last_col {
                        break;
                    }
                    let grapheme = item.bytes(&span.text);
                    let w = u8::try_from(ctx.string_width(grapheme)).unwrap_or(1).max(1);
                    let style = Style { bg, ..span.style };
                    surface.write_cell(
                        col,
                        last_row,
                        Cell {
                            char: Character::new(grapheme, w),
                            style,
                            ..Cell::default()
                        },
                    );
                    col = col.saturating_add(u16::from(w));
                }
            }
            surface.write_cell(col, last_row, border_cell(" "));
            col += 1;
        }
        while col < last_col {
            surface.write_cell(col, last_row, border_cell("\u{2501}"));
            col += 1;
        }
    }
}
