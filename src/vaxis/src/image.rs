//! Image geometry and kitty-graphics transmission.
//!
//! This module currently holds only the value types that [`crate::cell::Cell`]
//! references through `Cell.image`. The image-placement geometry (`draw`,
//! `cell_size`) and the kitty-graphics transmission path land in Phase 6.

/// Where an image's encoded bytes come from when transmitting to the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Path(String),
    Mem(Vec<u8>),
}

/// Pixel format used to transmit an image to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransmitFormat {
    Rgb,
    Rgba,
    Png,
}

/// Transport used to hand image bytes to the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransmitMedium {
    File,
    TempFile,
    SharedMem,
}

/// A request to draw an already-transmitted image into a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Placement {
    /// Unique identifier for the image, managed by the screen.
    pub img_id: u32,
    pub options: DrawOptions,
}

/// The size of an image expressed in terminal cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellSize {
    pub rows: u16,
    pub cols: u16,
}

/// Options controlling how a [`Placement`] is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrawOptions {
    /// Offset into the top-left cell, in pixels, locating the image origin.
    /// Must be less than the pixel size of a single cell.
    pub pixel_offset: Option<PixelOffset>,
    /// Vertical stacking order. Negative values draw beneath text, values
    /// below `-1_073_741_824` draw beneath default-background cells.
    pub z_index: Option<i32>,
    /// A clip region of the source image to draw.
    pub clip_region: Option<ClipRegion>,
    /// Scaling to apply to the image.
    pub scale: Scale,
    /// The size to render the image at. Prefer `scale`. The draw path fills
    /// this in with the correct values when a scale method is applied.
    pub size: Option<Size>,
}

/// Sub-cell pixel offset for an image origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PixelOffset {
    pub x: u16,
    pub y: u16,
}

/// A clip region of a source image, in pixels. `None` fields are unclipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ClipRegion {
    pub x: Option<u16>,
    pub y: Option<u16>,
    pub width: Option<u16>,
    pub height: Option<u16>,
}

/// An explicit render size in cells. `None` fields are derived by the draw
/// path from the chosen [`Scale`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub rows: Option<u16>,
    pub cols: Option<u16>,
}

/// How an image is scaled to its window.
///
/// NOTE: This is the image-scaling method, distinct from
/// [`crate::cell::Scale`], which scales text glyphs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Scale {
    /// No scaling applied. The image may extend beyond the window.
    #[default]
    None,
    /// Stretch or shrink the image to fill the window.
    Fill,
    /// Scale the image to fit the window, maintaining aspect ratio.
    Fit,
    /// Scale the image to fit the window, only if needed.
    Contain,
}
