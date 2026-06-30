//! Image geometry and kitty-graphics transmission.
//!
//! This module holds the value types that [`crate::cell::Cell`] references
//! through `Cell.image`, plus the [`Image`] handle and its placement geometry
//! (`draw`, `cell_size`). The kitty-graphics transmission path (allocating
//! image ids, base64-chunking the payload) lives on
//! [`Vaxis`](crate::vaxis::Vaxis), which owns the id counter and the writer.

use crate::error::Error;
use crate::window::Window;

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

/// A transmitted image: its terminal-assigned id and pixel dimensions.
///
/// An `Image` is a handle to bytes already living in the terminal's graphics
/// memory. It carries the geometry needed to place that image into a
/// [`Window`] ([`Image::draw`]) and to measure its cell footprint
/// ([`Image::cell_size`]). Construct one through the `Vaxis` transmission
/// methods, which allocate the id.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Image {
    id: u32,
    width: u16,
    height: u16,
}

impl Image {
    /// Builds a handle for an image the terminal has stored under `id` with the
    /// given pixel dimensions. Crate-internal: ids are allocated by the
    /// `Vaxis` transmission methods.
    pub(crate) fn new(id: u32, width: u16, height: u16) -> Self {
        Self { id, width, height }
    }

    /// The terminal-assigned image id.
    pub fn id(&self) -> u32 {
        self.id
    }

    /// Image width in pixels.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Image height in pixels.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Places this image at the window's `(0, 0)` cell, deriving the render
    /// size from `opts.scale`.
    ///
    /// The scale methods map to a [`Size`] written into the placement:
    /// `None` leaves it unset, `Fill` stretches to the window's rows and cols,
    /// and `Fit`/`Contain` pick the dimension to constrain by comparing the
    /// image against the window's pixel extent (cell count times pixels per
    /// cell). `Contain` is a no-op when the image already fits.
    pub fn draw(&self, win: &Window<'_>, opts: DrawOptions) -> Result<(), Error> {
        let mut p_opts = opts;
        match opts.scale {
            Scale::None => {}
            Scale::Fill => {
                p_opts.size = Some(Size {
                    rows: Some(win.height),
                    cols: Some(win.width),
                });
            }
            Scale::Fit | Scale::Contain => {
                // The window's pixel extent is its cell count times the
                // (ceil-rounded) pixels per cell of the underlying screen.
                let pix_per_col =
                    usize::from(div_ceil(win.screen_width_pix(), win.screen_width())?);
                let pix_per_row =
                    usize::from(div_ceil(win.screen_height_pix(), win.screen_height())?);
                let win_width_pix = pix_per_col * usize::from(win.width);
                let win_height_pix = pix_per_row * usize::from(win.height);

                let fit_x = win_width_pix >= usize::from(self.width);
                let fit_y = win_height_pix >= usize::from(self.height);

                let scale_rows = Some(Size {
                    rows: Some(win.height),
                    cols: None,
                });
                let scale_cols = Some(Size {
                    rows: None,
                    cols: Some(win.width),
                });

                if opts.scale == Scale::Contain && fit_x && fit_y {
                    // Already fits with no scaling: leave the size unset.
                } else if fit_x && !fit_y {
                    p_opts.size = scale_rows;
                } else if !fit_x && fit_y {
                    p_opts.size = scale_cols;
                } else if !fit_x && !fit_y {
                    // Too big in both directions: constrain the dimension that
                    // overshoots the most.
                    let diff_x = usize::from(self.width) - win_width_pix;
                    let diff_y = usize::from(self.height) - win_height_pix;
                    p_opts.size = if diff_x > diff_y {
                        scale_cols
                    } else {
                        scale_rows
                    };
                } else {
                    // Fits in both directions, so this can only be `Fit` (the
                    // `Contain`-already-fits case broke out above). Constrain
                    // the dimension with the least slack.
                    debug_assert!(opts.scale == Scale::Fit);
                    let diff_x = win_width_pix - usize::from(self.width);
                    let diff_y = win_height_pix - usize::from(self.height);
                    p_opts.size = if diff_x < diff_y {
                        scale_cols
                    } else {
                        scale_rows
                    };
                }
            }
        }
        win.write_cell(
            0,
            0,
            crate::cell::Cell {
                image: Some(Placement {
                    img_id: self.id,
                    options: p_opts,
                }),
                ..crate::cell::Cell::default()
            },
        );
        Ok(())
    }

    /// The image's footprint in terminal cells, rounded up.
    ///
    /// Errors with [`Error::ZeroSizedScreen`] when the screen has zero cell
    /// width or height, since pixels-per-cell divides by those.
    pub fn cell_size(&self, win: &Window<'_>) -> Result<CellSize, Error> {
        let pix_per_col = div_ceil(win.screen_width_pix(), win.screen_width())?;
        let pix_per_row = div_ceil(win.screen_height_pix(), win.screen_height())?;
        // A screen narrower than one cell-pixel (pix_per_* == 0) yields a zero
        // footprint rather than an error, matching upstream's `catch 0`.
        let cols = if pix_per_col == 0 {
            0
        } else {
            self.width.div_ceil(pix_per_col)
        };
        let rows = if pix_per_row == 0 {
            0
        } else {
            self.height.div_ceil(pix_per_row)
        };
        Ok(CellSize { rows, cols })
    }
}

/// Ceiling division that surfaces a zero divisor as [`Error::ZeroSizedScreen`],
/// matching the error path of Zig's `std.math.divCeil`.
fn div_ceil(a: u16, b: u16) -> Result<u16, Error> {
    if b == 0 {
        return Err(Error::ZeroSizedScreen);
    }
    Ok(a.div_ceil(b))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;
    use crate::Winsize;
    use crate::screen::Screen;

    /// A 10x10-cell, 100x100-pixel screen: 10 pixels per cell each way.
    fn screen_10x10() -> RefCell<Screen> {
        RefCell::new(Screen::new(Winsize {
            rows: 10,
            cols: 10,
            x_pixel: 100,
            y_pixel: 100,
        }))
    }

    fn full_window(screen: &RefCell<Screen>) -> Window<'_> {
        Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 10,
            height: 10,
            screen,
        }
    }

    fn drawn_placement(win: &Window<'_>) -> Placement {
        win.read_cell(0, 0)
            .expect("cell (0,0)")
            .image
            .expect("placement written to (0,0)")
    }

    #[test]
    fn draw_fill_uses_window_rows_and_cols() {
        let screen = screen_10x10();
        let win = full_window(&screen);
        let img = Image::new(5, 1000, 1000);
        img.draw(
            &win,
            DrawOptions {
                scale: Scale::Fill,
                ..DrawOptions::default()
            },
        )
        .expect("draw");
        let p = drawn_placement(&win);
        assert_eq!(p.img_id, 5);
        assert_eq!(
            p.options.size,
            Some(Size {
                rows: Some(10),
                cols: Some(10),
            })
        );
    }

    #[test]
    fn draw_fit_scales_by_largest_overshoot() {
        let screen = screen_10x10();
        let win = full_window(&screen);
        // 100x100 window pixels. Image overshoots both, width more (200) than
        // height (50), so it constrains columns.
        let img = Image::new(1, 300, 150);
        img.draw(
            &win,
            DrawOptions {
                scale: Scale::Fit,
                ..DrawOptions::default()
            },
        )
        .expect("draw");
        assert_eq!(
            drawn_placement(&win).options.size,
            Some(Size {
                rows: None,
                cols: Some(10),
            })
        );
    }

    #[test]
    fn draw_fit_scales_rows_when_only_height_overshoots() {
        let screen = screen_10x10();
        let win = full_window(&screen);
        // Fits horizontally (50 <= 100), overshoots vertically (200 > 100).
        let img = Image::new(1, 50, 200);
        img.draw(
            &win,
            DrawOptions {
                scale: Scale::Fit,
                ..DrawOptions::default()
            },
        )
        .expect("draw");
        assert_eq!(
            drawn_placement(&win).options.size,
            Some(Size {
                rows: Some(10),
                cols: None,
            })
        );
    }

    #[test]
    fn draw_contain_no_op_when_image_fits() {
        let screen = screen_10x10();
        let win = full_window(&screen);
        // Smaller than the window in both directions: contain leaves size unset.
        let img = Image::new(1, 50, 50);
        img.draw(
            &win,
            DrawOptions {
                scale: Scale::Contain,
                ..DrawOptions::default()
            },
        )
        .expect("draw");
        assert_eq!(drawn_placement(&win).options.size, None);
    }

    #[test]
    fn draw_none_leaves_size_unset() {
        let screen = screen_10x10();
        let win = full_window(&screen);
        let img = Image::new(1, 9999, 9999);
        img.draw(&win, DrawOptions::default()).expect("draw");
        assert_eq!(drawn_placement(&win).options.size, None);
    }

    #[test]
    fn cell_size_rounds_up() {
        let screen = screen_10x10();
        let win = full_window(&screen);
        // 10 pixels per cell: 25px wide -> 3 cols, 35px tall -> 4 rows.
        let img = Image::new(1, 25, 35);
        assert_eq!(
            img.cell_size(&win).expect("cell size"),
            CellSize { rows: 4, cols: 3 }
        );
    }

    #[test]
    fn cell_size_errors_on_zero_sized_screen() {
        let screen = RefCell::new(Screen::default());
        let win = Window {
            x_off: 0,
            y_off: 0,
            parent_x_off: 0,
            parent_y_off: 0,
            width: 0,
            height: 0,
            screen: &screen,
        };
        let img = Image::new(1, 10, 10);
        assert!(matches!(img.cell_size(&win), Err(Error::ZeroSizedScreen)));
    }
}
