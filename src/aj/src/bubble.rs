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
use vaxis::image::{DrawOptions, Placement, Size as ImageSize};
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

/// Upper bound on an inline image's cell footprint: `(max cols, max rows)`.
/// An image larger than this is scaled down to fit, preserving aspect ratio,
/// so a big screenshot never floods the transcript.
pub(crate) const MAX_IMAGE_CELLS: (u32, u32) = (40, 20);

/// Compute an image's cell-grid footprint, scaled to fit `max_cells` while
/// preserving aspect ratio.
///
/// `image_px` is the displayed image size in pixels, `cell_px` is the
/// terminal's pixels-per-cell, and `max_cells` is `(max cols, max rows)`.
/// Returns `(cols, rows)`, always at least `(1, 1)`. Any zero input component
/// (a degenerate image or a screen reporting no cell pixels) yields `(1, 1)`.
///
/// The base grid is the 1:1 pixel-to-cell footprint. If either axis exceeds
/// its max, both axes scale by the smaller of the two per-axis ratios, so the
/// aspect ratio holds. This mirrors the shared reference footprint algorithm
/// so both frontends render an image at the same cell size.
pub(crate) fn image_cell_footprint(
    image_px: (u32, u32),
    cell_px: (u32, u32),
    max_cells: (u32, u32),
) -> (u32, u32) {
    let (iw, ih) = image_px;
    let (cw, ch) = cell_px;
    let (max_cols, max_rows) = max_cells;
    if iw == 0 || ih == 0 || cw == 0 || ch == 0 || max_cols == 0 || max_rows == 0 {
        return (1, 1);
    }
    let base_cols = iw.div_ceil(cw).max(1);
    let base_rows = ih.div_ceil(ch).max(1);
    // Scale down preserving aspect ratio if either axis overflows. f32 is
    // plenty: terminal cell counts stay well within its integer precision.
    #[allow(clippy::as_conversions)]
    let scale_cols = if base_cols > max_cols {
        max_cols as f32 / base_cols as f32
    } else {
        1.0
    };
    #[allow(clippy::as_conversions)]
    let scale_rows = if base_rows > max_rows {
        max_rows as f32 / base_rows as f32
    } else {
        1.0
    };
    let scale = scale_cols.min(scale_rows);
    #[allow(clippy::as_conversions)]
    let cols = ((base_cols as f32) * scale).floor().max(1.0) as u32;
    #[allow(clippy::as_conversions)]
    let rows = ((base_rows as f32) * scale).floor().max(1.0) as u32;
    (cols.min(max_cols), rows.min(max_rows))
}

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

/// An inline image block reserved below a bubble's text content.
///
/// `px` is the image's displayed size in pixels, used to compute the cell
/// footprint at draw time. `img_id` is the terminal-assigned id of the
/// transmitted image, or `None` while the transmit is still pending: the
/// bubble reserves the footprint's rows either way, so the image popping in
/// one frame later does not shift the layout.
#[derive(Clone, Copy)]
pub(crate) struct BubbleImage {
    pub(crate) px: (u32, u32),
    pub(crate) img_id: Option<u32>,
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
    /// An inline image reserved below the text, on the tinted path only.
    /// `None` on every bubble without an image.
    pub(crate) image: Option<BubbleImage>,
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
            image: None,
        }
    }

    /// Attach the focus border painted into the padding frame. Only the real
    /// (tinted) bubble path honors it. The plain fallback ignores it.
    pub(crate) fn with_border(mut self, border: BubbleBorder) -> Bubble {
        self.border = Some(border);
        self
    }

    /// Attach an inline image block reserved below the text. Only the tinted
    /// bubble path draws it. The plain (`header_only`) fallback ignores it.
    pub(crate) fn with_image(mut self, image: BubbleImage) -> Bubble {
        self.image = Some(image);
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

        // Reserve the rows an inline image needs below the text. The footprint
        // comes from the displayed pixel size, known without transmitting, so
        // we reserve the rows whether or not the id has arrived. Reserving
        // before the id lands keeps the bubble height stable, so the image
        // popping in one frame later does not shift the layout.
        let text_height = inner.size.height;
        let image_cells = self.image.map(|img| {
            // Clamp the footprint's column budget to the bubble's inner width
            // inside the aspect-preserving math, so a narrow terminal scales
            // both axes together instead of squashing only the width and
            // stretching the height.
            let max_cols = MAX_IMAGE_CELLS.0.min(u32::from(inner_width));
            let (cols, rows) = image_cell_footprint(
                img.px,
                (
                    u32::from(ctx.cell_size.width),
                    u32::from(ctx.cell_size.height),
                ),
                (max_cols, MAX_IMAGE_CELLS.1),
            );
            let cols = u16::try_from(cols).unwrap_or(u16::MAX);
            let rows = u16::try_from(rows).unwrap_or(u16::MAX);
            (cols, rows, img.img_id)
        });
        let image_rows = image_cells.map_or(0, |(_, rows, _)| rows);

        // The outer surface: bg-filled padding frame around the
        // content, plus (for transcript entries) one default
        // (untinted) spacer row at the bottom standing in for the
        // `\n\n` spacer the span-based entries carry.
        let content_height = text_height + image_rows;
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
        // Place the transmitted image at the origin of its reserved rows,
        // below the text. One cell carries the placement; the terminal renders
        // the image spanning `rows`x`cols` cells from here. Written after the
        // bg fill and the text sub-surface so nothing overwrites it. When the
        // id is not yet transmitted the reserved rows stay bg-filled blank.
        if let Some((cols, rows, Some(id))) = image_cells {
            surface.write_cell(
                PADDING_X,
                PADDING_Y + text_height,
                Cell {
                    image: Some(Placement {
                        img_id: id,
                        options: DrawOptions {
                            size: Some(ImageSize {
                                rows: Some(rows),
                                cols: Some(cols),
                            }),
                            ..DrawOptions::default()
                        },
                    }),
                    style: bg_style,
                    ..Cell::default()
                },
            );
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{draw_ctx, flatten};

    #[test]
    fn footprint_preserves_aspect_on_downscale() {
        // 1000x500 at 10x10 cells is base (100, 50); capping at (50, 25)
        // scales by 0.5, landing at (50, 25) with the 2:1 aspect held.
        let (cols, rows) = image_cell_footprint((1000, 500), (10, 10), (50, 25));
        assert!(cols <= 50 && rows <= 25, "within cap: {cols}x{rows}");
        assert!(
            cols >= 2 * rows - 2 && cols <= 2 * rows + 2,
            "{cols}x{rows}"
        );
    }

    #[test]
    fn footprint_caps_the_larger_axis() {
        // 4000x4000 at 10x10 cells is base (400, 400). The row cap (20) is
        // tighter than the col cap (40), so both scale by 20/400 = 0.05.
        let (cols, rows) = image_cell_footprint((4000, 4000), (10, 10), (40, 20));
        assert!(cols <= 40 && rows <= 20, "within cap: {cols}x{rows}");
        assert_eq!(rows, 20, "the tighter row cap binds");
    }

    #[test]
    fn footprint_minimum_is_one_by_one() {
        assert_eq!(image_cell_footprint((1, 1), (9, 18), (40, 20)), (1, 1));
    }

    #[test]
    fn footprint_zero_input_yields_one_by_one() {
        assert_eq!(image_cell_footprint((0, 0), (9, 18), (40, 20)), (1, 1));
        assert_eq!(image_cell_footprint((100, 100), (0, 18), (40, 20)), (1, 1));
        assert_eq!(image_cell_footprint((100, 100), (9, 18), (40, 0)), (1, 1));
    }

    /// A one-line tinted bubble with `image` set, drawn at cell size (10, 20).
    fn image_bubble(img_id: Option<u32>, px: (u32, u32)) -> Surface {
        let text = vec![TextSpan {
            text: "hi".into(),
            ..TextSpan::default()
        }];
        let mut bubble = Bubble::entry(text, Some(Color::Rgb([1, 2, 3])), Style::default())
            .with_image(BubbleImage { px, img_id });
        bubble.draw(&draw_ctx(60, None))
    }

    #[test]
    fn image_block_with_id_grows_height_and_places_the_image() {
        // px (100, 80) at cell (10, 20) is base (10, 4), under the cap, so the
        // footprint is (10, 4): 4 reserved rows.
        let surface = image_bubble(Some(7), (100, 80));
        // Baseline height without an image: one text row + two pad rows + one
        // trailing spacer = 4. The four image rows grow it to 8.
        assert_eq!(surface.size.height, 8, "height grew by the 4 image rows");

        // The placement lands at the image origin: col PADDING_X, row past the
        // top pad and the single text row (PADDING_Y + text_height = 2).
        let grid = flatten(&surface);
        let placement = grid[usize::from(PADDING_Y) + 1][usize::from(PADDING_X)]
            .image
            .expect("placement cell at the image origin");
        assert_eq!(placement.img_id, 7);
        assert_eq!(
            placement.options.size,
            Some(ImageSize {
                rows: Some(4),
                cols: Some(10),
            }),
        );
    }

    #[test]
    fn image_block_without_id_reserves_rows_but_places_nothing() {
        let surface = image_bubble(None, (100, 80));
        // Same reserved footprint, so the height matches the placed case: the
        // image popping in later must not shift the layout.
        assert_eq!(surface.size.height, 8, "rows reserved without the id");
        let grid = flatten(&surface);
        assert!(
            grid.iter().flatten().all(|cell| cell.image.is_none()),
            "no placement cell while the transmit is pending",
        );
    }

    #[test]
    fn narrow_bubble_scales_both_axes_preserving_aspect() {
        // A 400x400 image at cell (10, 20) is base (40, 20). Drawn at width 30
        // the inner width is 28, tighter than the 40 columns the image wants.
        // Clamping inside the aspect math scales both axes by 28/40: cols land
        // at 28 (== inner width) and rows fall from 20 to 14, not the full
        // height. Without the clamp riding the math the width would squash to
        // 28 while rows stayed 20, stretching the image.
        let text = vec![TextSpan {
            text: "hi".into(),
            ..TextSpan::default()
        }];
        let mut bubble = Bubble::entry(text, Some(Color::Rgb([1, 2, 3])), Style::default())
            .with_image(BubbleImage {
                px: (400, 400),
                img_id: Some(3),
            });
        let surface = bubble.draw(&draw_ctx(30, None));
        let placement = flatten(&surface)
            .into_iter()
            .flatten()
            .find_map(|c| c.image)
            .expect("placement cell");
        assert_eq!(
            placement.options.size,
            Some(ImageSize {
                rows: Some(14),
                cols: Some(28),
            }),
            "both axes scaled to the 28-col inner width, 2:1 aspect held",
        );
    }
}
