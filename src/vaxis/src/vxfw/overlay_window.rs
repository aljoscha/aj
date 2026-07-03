//! [`OverlayWindow`]: titled, bordered chrome around an overlay's content.
//!
//! Purely visual: a rounded frame with an inline title on the top edge, an
//! optional right-aligned subtitle (typically a key hint) on the bottom edge,
//! and one blank padding row above and below the child. All input, focus, and
//! outcome behavior belongs to the child.

use crate::cell::{Cell, Character, Style};
use crate::vxfw::{
    DrawContext, MaxSize, RelativePoint, Size, SubSurface, Surface, Widget, WidgetRef, draw_widget,
};

/// Rows the frame adds around the child: top and bottom border plus the top
/// and bottom blank padding rows.
pub const OVERLAY_WINDOW_CHROME_ROWS: u16 = 4;

/// Columns the frame adds around the child: the two border columns plus one
/// space of padding on each side.
pub const OVERLAY_WINDOW_CHROME_COLS: u16 = 4;

/// A titled, bordered overlay container.
///
/// Draws at exactly the bounded max size the caller passes (the overlay host
/// computes placement and size, the window just fills it). The child is
/// constrained to the interior, `chrome` rows/cols smaller, and positioned at
/// `(2, 2)`.
pub struct OverlayWindow {
    pub child: WidgetRef,
    /// Inline label on the top edge, inset one dash from the left corner.
    pub title: String,
    /// Inline label on the bottom edge, inset one dash from the right corner.
    /// Empty means no subtitle.
    pub subtitle: String,
    pub border_style: Style,
    pub title_style: Style,
    pub subtitle_style: Style,
}

impl OverlayWindow {
    /// A window around `child` with default styles and no subtitle.
    pub fn new(title: impl Into<String>, child: WidgetRef) -> OverlayWindow {
        OverlayWindow {
            child,
            title: title.into(),
            subtitle: String::new(),
            border_style: Style::default(),
            title_style: Style::default(),
            subtitle_style: Style::default(),
        }
    }
}

/// Where an edge's inline label sits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EdgeAlign {
    Left,
    Right,
}

impl OverlayWindow {
    /// Draws one horizontal edge with corners, dashes, and an optional inline
    /// label. The label gets one space of breathing room on each side plus one
    /// dash between it and the nearer corner, and is truncated with an
    /// ellipsis when the edge is too narrow. Below five interior columns the
    /// label is omitted entirely.
    #[allow(clippy::too_many_arguments)]
    fn write_edge(
        &self,
        surf: &mut Surface,
        ctx: &DrawContext,
        row: u16,
        corners: (&str, &str),
        label: &str,
        label_style: Style,
        align: EdgeAlign,
    ) {
        let width = surf.size.width;
        let border = |g: &str| Cell {
            char: Character::new(g, 1),
            style: self.border_style,
            ..Cell::default()
        };
        if width < 2 {
            for col in 0..width {
                surf.write_cell(col, row, border("─"));
            }
            return;
        }
        surf.write_cell(0, row, border(corners.0));
        for col in 1..width - 1 {
            surf.write_cell(col, row, border("─"));
        }
        surf.write_cell(width - 1, row, border(corners.1));

        let interior = usize::from(width) - 2;
        if label.is_empty() || interior < 5 {
            return;
        }
        let max_label = interior - 4;
        let (shown, shown_width) = truncate_to_width(ctx, label, max_label);
        let start = match align {
            EdgeAlign::Left => 2usize,
            EdgeAlign::Right => usize::from(width) - 4 - shown_width,
        };
        let start = u16::try_from(start).expect("edge label start fits u16");
        // The label block is ` label ` (spaces included) in the label style.
        let styled = |g: &str, w: u8| Cell {
            char: Character::new(g, w),
            style: label_style,
            ..Cell::default()
        };
        surf.write_cell(start, row, styled(" ", 1));
        let mut col = start + 1;
        for grapheme in ctx.grapheme_iterator(&shown) {
            let g = grapheme.bytes(&shown);
            let w = u8::try_from(ctx.string_width(g)).expect("grapheme width fits u8");
            surf.write_cell(col, row, styled(g, w));
            col += u16::from(w);
        }
        surf.write_cell(col, row, styled(" ", 1));
    }
}

/// Truncates `label` to at most `max_width` columns, appending `…` when
/// anything was cut. Returns the shown string and its display width.
fn truncate_to_width(ctx: &DrawContext, label: &str, max_width: usize) -> (String, usize) {
    let full = ctx.string_width(label);
    if full <= max_width {
        return (label.to_string(), full);
    }
    if max_width == 0 {
        return (String::new(), 0);
    }
    let mut out = String::new();
    let mut width = 0usize;
    for grapheme in ctx.grapheme_iterator(label) {
        let g = grapheme.bytes(label);
        let w = ctx.string_width(g);
        // Reserve one column for the ellipsis.
        if width + w > max_width - 1 {
            break;
        }
        out.push_str(g);
        width += w;
    }
    out.push('…');
    (out, width + 1)
}

impl Widget for OverlayWindow {
    fn draw(&mut self, ctx: &DrawContext) -> Surface {
        // The host's placement math decides the window's exact footprint, so
        // we fill the bounded max instead of sizing to the child.
        let size = ctx.max.size();
        let mut surf = Surface::with_size(size);

        let inner_width = size.width.saturating_sub(OVERLAY_WINDOW_CHROME_COLS);
        let inner_height = size.height.saturating_sub(OVERLAY_WINDOW_CHROME_ROWS);
        if inner_width > 0 && inner_height > 0 {
            let child_ctx = ctx.with_constraints(
                Size {
                    width: 0,
                    height: 0,
                },
                MaxSize {
                    width: Some(inner_width),
                    height: Some(inner_height),
                },
            );
            surf.children.push(SubSurface {
                origin: RelativePoint { row: 2, col: 2 },
                surface: draw_widget(&self.child, &child_ctx),
                z_index: 0,
            });
        }

        if size.height >= 1 {
            let title = self.title.clone();
            self.write_edge(
                &mut surf,
                ctx,
                0,
                ("╭", "╮"),
                &title,
                self.title_style,
                EdgeAlign::Left,
            );
        }
        if size.height >= 2 {
            let subtitle = self.subtitle.clone();
            self.write_edge(
                &mut surf,
                ctx,
                size.height - 1,
                ("╰", "╯"),
                &subtitle,
                self.subtitle_style,
                EdgeAlign::Right,
            );
        }
        let vertical = Cell {
            char: Character::new("│", 1),
            style: self.border_style,
            ..Cell::default()
        };
        for row in 1..size.height.saturating_sub(1) {
            surf.write_cell(0, row, vertical.clone());
            if size.width >= 2 {
                surf.write_cell(size.width - 1, row, vertical.clone());
            }
        }
        surf
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use crate::gwidth;
    use crate::vxfw::Text;

    fn draw_ctx(width: u16, height: u16) -> DrawContext {
        DrawContext {
            min: Size {
                width: 0,
                height: 0,
            },
            max: MaxSize {
                width: Some(width),
                height: Some(height),
            },
            cell_size: Size {
                width: 10,
                height: 20,
            },
            width_method: gwidth::Method::Unicode,
        }
    }

    /// Reads a row's graphemes as a plain string.
    fn row_text(surf: &Surface, row: u16) -> String {
        (0..surf.size.width)
            .map(|col| surf.read_cell(col, row).char.grapheme().to_string())
            .collect()
    }

    #[test]
    fn overlay_window() {
        let child: WidgetRef = Rc::new(RefCell::new(Text::new("hello")));
        let mut win = OverlayWindow::new("Commands", Rc::clone(&child));
        win.subtitle = "Enter to confirm".to_string();

        let ctx = draw_ctx(40, 10);
        let surf = win.draw(&ctx);

        // Fills the bounded max exactly.
        assert_eq!(surf.size.width, 40);
        assert_eq!(surf.size.height, 10);

        // Top edge: corner, dash, then the title inset with spaces.
        let top = row_text(&surf, 0);
        assert!(top.starts_with("╭─ Commands ─"), "top: {top:?}");
        assert!(top.ends_with('╮'), "top: {top:?}");

        // Bottom edge: subtitle right-aligned, one dash before the corner.
        let bottom = row_text(&surf, 9);
        assert!(bottom.starts_with('╰'), "bottom: {bottom:?}");
        assert!(
            bottom.ends_with("─ Enter to confirm ─╯"),
            "bottom: {bottom:?}"
        );

        // Vertical borders on the interior rows.
        for row in 1..9 {
            assert_eq!(surf.read_cell(0, row).char.grapheme(), "│");
            assert_eq!(surf.read_cell(39, row).char.grapheme(), "│");
        }

        // The child sits at (2, 2), constrained to the interior.
        assert_eq!(surf.children.len(), 1);
        let inner = &surf.children[0];
        assert_eq!(inner.origin, RelativePoint { row: 2, col: 2 });
        assert!(inner.surface.size.width <= 36);
        assert!(inner.surface.size.height <= 6);
    }

    #[test]
    fn long_title_is_truncated_with_an_ellipsis() {
        let child: WidgetRef = Rc::new(RefCell::new(Text::new("x")));
        let mut win = OverlayWindow::new("A very long overlay window title", child);
        let surf = win.draw(&draw_ctx(16, 6));
        let top = row_text(&surf, 0);
        assert!(top.contains('…'), "top: {top:?}");
        assert!(top.starts_with('╭') && top.ends_with('╮'), "top: {top:?}");
    }

    #[test]
    fn narrow_window_omits_labels_and_child() {
        let child: WidgetRef = Rc::new(RefCell::new(Text::new("x")));
        let mut win = OverlayWindow::new("Title", child);
        win.subtitle = "hint".to_string();
        let surf = win.draw(&draw_ctx(4, 3));
        // Too narrow for a label (interior < 5) and for a child (inner
        // width would be zero), but the frame itself still renders.
        assert_eq!(row_text(&surf, 0), "╭──╮");
        assert_eq!(row_text(&surf, 2), "╰──╯");
        assert!(surf.children.is_empty());
    }
}
