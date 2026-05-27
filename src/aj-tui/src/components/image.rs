//! Inline image component.
//!
//! Renders an image as one or more rows of `Vec<String>`. When the
//! host terminal advertises an [`ImageProtocol`] the component
//! emits the corresponding escape; otherwise it falls back to a
//! single muted-text placeholder. See
//! [`crate::image_protocol`] for the protocol-specific encoding
//! and the multi-row layout contract the differential renderer
//! relies on.
//!
//! The component is intentionally read-only — `handle_input`
//! always returns `false`. Callers manage the image's lifetime by
//! adding / removing the component from a parent container.
//!
//! [`ImageProtocol`]: crate::capabilities::ImageProtocol

use std::any::Any;

use crate::capabilities::{ImageProtocol, get_capabilities};
use crate::component::Component;
use crate::image_protocol::{
    DEFAULT_CELL_PIXEL_SIZE, image_cell_footprint, iterm2_sequence, kitty_sequence,
    next_kitty_image_id,
};
use crate::keys::InputEvent;
use crate::style;

/// Inline image bound to a base64 payload and a target cell-grid
/// footprint.
///
/// Render-time the component consults
/// [`crate::capabilities::get_capabilities`] and emits either the
/// Kitty graphics sequence, the iTerm2 OSC 1337 sequence, or a
/// single muted-text fallback row. The Kitty image ID is
/// allocated lazily so the same ID is reused across frames — the
/// differential renderer relies on stable IDs to delete previous
/// placements before redrawing the row.
pub struct Image {
    base64_data: String,
    mime_type: String,
    image_pixels: (u32, u32),
    max_cells: (u32, u32),
    cell_pixel: Option<(u32, u32)>,
    /// Allocated on first Kitty render and reused thereafter so
    /// the diff engine can match the placement across frames.
    kitty_image_id: Option<u32>,
}

impl Image {
    /// Construct an image bound to a base64 payload.
    ///
    /// `image_pixels` is the source dimensions; `max_cells` caps
    /// the rendered cell footprint (caller is responsible for
    /// choosing a sensible cap so a huge image doesn't take half
    /// the screen). `cell_pixel` is the per-cell pixel size
    /// reported by the terminal; pass `None` to use
    /// [`DEFAULT_CELL_PIXEL_SIZE`].
    pub fn new(
        base64_data: String,
        mime_type: String,
        image_pixels: (u32, u32),
        max_cells: (u32, u32),
        cell_pixel: Option<(u32, u32)>,
    ) -> Self {
        Self {
            base64_data,
            mime_type,
            image_pixels,
            max_cells,
            cell_pixel,
            kitty_image_id: None,
        }
    }

    fn fallback(&self) -> Vec<String> {
        let (w, h) = self.image_pixels;
        vec![style::dim(&format!(
            "[image: {} · {}x{}]",
            self.mime_type, w, h,
        ))]
    }
}

impl Component for Image {
    crate::impl_component_any!();

    fn render(&mut self, width: usize) -> Vec<String> {
        let caps = get_capabilities();
        let Some(protocol) = caps.images else {
            return self.fallback();
        };
        let cell = self.cell_pixel.unwrap_or(DEFAULT_CELL_PIXEL_SIZE);
        let (cols, rows) = image_cell_footprint(self.image_pixels, cell, self.max_cells);
        // Clamp to layout width so the image never overflows the
        // column budget the parent gave us.
        #[allow(clippy::as_conversions)]
        let cols = cols.min(width.max(1) as u32);
        let rows_usize = usize::try_from(rows.max(1)).unwrap_or(usize::MAX);

        match protocol {
            ImageProtocol::Kitty => {
                if self.mime_type != "image/png" {
                    // Kitty doesn't accept JPEG inline; fall back
                    // to the muted-text placeholder. TODO: transcode
                    // to PNG so the inline rendering path works.
                    return self.fallback();
                }
                let id = *self.kitty_image_id.get_or_insert_with(next_kitty_image_id);
                let escape = kitty_sequence(&self.base64_data, cols, rows, id);
                let mut out = Vec::with_capacity(rows_usize);
                out.push(escape);
                for _ in 1..rows_usize {
                    out.push(String::new());
                }
                out
            }
            ImageProtocol::ITerm2 => {
                let escape = iterm2_sequence(&self.base64_data, cols, rows);
                let mut out = Vec::with_capacity(rows_usize);
                for _ in 1..rows_usize {
                    out.push(String::new());
                }
                out.push(escape);
                out
            }
        }
    }

    fn handle_input(&mut self, _event: &InputEvent) -> bool {
        false
    }
}

impl AsRef<dyn Any> for Image {
    fn as_ref(&self) -> &(dyn Any + 'static) {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{TerminalCapabilities, reset_capabilities_cache, set_capabilities};
    use serial_test::serial;

    fn make(mime: &str) -> Image {
        Image::new(
            "ZGF0YQ==".to_string(),
            mime.to_string(),
            (100, 50),
            (40, 20),
            Some((10, 10)),
        )
    }

    #[test]
    #[serial]
    fn fallback_when_no_image_protocol() {
        set_capabilities(TerminalCapabilities {
            hyperlinks: false,
            true_color: false,
            images: None,
        });
        let mut img = make("image/png");
        let lines = img.render(80);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("[image: image/png · 100x50]"));
        reset_capabilities_cache();
    }

    #[test]
    #[serial]
    fn kitty_png_produces_rows_with_escape_on_first_row() {
        set_capabilities(TerminalCapabilities {
            hyperlinks: false,
            true_color: false,
            images: Some(ImageProtocol::Kitty),
        });
        let mut img = make("image/png");
        let lines = img.render(80);
        assert!(lines.len() >= 2, "expected multi-row, got {lines:?}");
        assert!(lines[0].contains("\x1b_G"));
        for tail in &lines[1..] {
            assert!(tail.is_empty(), "trailing rows should be empty");
        }
        reset_capabilities_cache();
    }

    #[test]
    #[serial]
    fn kitty_jpeg_falls_back_to_text() {
        set_capabilities(TerminalCapabilities {
            hyperlinks: false,
            true_color: false,
            images: Some(ImageProtocol::Kitty),
        });
        let mut img = make("image/jpeg");
        let lines = img.render(80);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("[image:"));
        reset_capabilities_cache();
    }

    #[test]
    #[serial]
    fn iterm2_places_escape_on_last_row_with_cursor_up() {
        set_capabilities(TerminalCapabilities {
            hyperlinks: false,
            true_color: false,
            images: Some(ImageProtocol::ITerm2),
        });
        let mut img = make("image/png");
        let lines = img.render(80);
        assert!(lines.len() >= 2, "expected multi-row, got {lines:?}");
        let last = lines.last().expect("non-empty");
        assert!(last.contains("\x1b]1337;File="));
        let n = lines.len();
        assert!(
            last.starts_with(&format!("\x1b[{}A", n - 1)),
            "expected cursor-up prefix for N-1 rows: {last:?}",
        );
        for head in &lines[..n - 1] {
            assert!(head.is_empty(), "leading rows should be empty");
        }
        reset_capabilities_cache();
    }

    #[test]
    #[serial]
    fn iterm2_single_row_has_no_cursor_up_prefix() {
        set_capabilities(TerminalCapabilities {
            hyperlinks: false,
            true_color: false,
            images: Some(ImageProtocol::ITerm2),
        });
        // Tiny 1x1 image → single-row footprint.
        let mut img = Image::new(
            "ZGF0YQ==".to_string(),
            "image/png".to_string(),
            (1, 1),
            (40, 20),
            Some((10, 10)),
        );
        let lines = img.render(80);
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].starts_with("\x1b["));
        assert!(lines[0].starts_with("\x1b]1337;File="));
        reset_capabilities_cache();
    }
}
