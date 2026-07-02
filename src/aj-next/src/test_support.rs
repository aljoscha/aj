//! Shared test helpers for inspecting drawn surfaces.

use vaxis::cell::Cell;
use vaxis::vxfw::{DrawContext, MaxSize, Size, Surface};

use crate::subagent_box::surface_rows;

/// A draw context bounded to `width`, optionally bounded in height.
pub(crate) fn draw_ctx(width: u16, height: Option<u16>) -> DrawContext {
    DrawContext {
        min: Size {
            width: 0,
            height: 0,
        },
        max: MaxSize {
            width: Some(width),
            height,
        },
        cell_size: Size {
            width: 10,
            height: 20,
        },
        width_method: vaxis::gwidth::Method::Unicode,
    }
}

/// Composite a surface tree into a flat cell grid, the way
/// `Surface::render` paints it.
pub(crate) fn flatten(surface: &Surface) -> Vec<Vec<Cell>> {
    surface_rows(surface)
}

/// The visible text of each composited row, right-trimmed.
pub(crate) fn rows(surface: &Surface) -> Vec<String> {
    flatten(surface)
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| c.char.grapheme())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}
