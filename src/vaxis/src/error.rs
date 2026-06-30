//! The error type returned by the [`Vaxis`](crate::vaxis::Vaxis) runtime.

use thiserror::Error;

/// Errors returned by the [`Vaxis`](crate::vaxis::Vaxis) runtime and its
/// byte-emitting methods.
///
/// [`Error::Io`] is the common case: a write failure on the writer the runtime
/// emits to. The remaining variants are the runtime's logical failures, which a
/// caller can match on to recover.
// NOTE: This set is open and grows. The `#[from] io::Error` bridge is
// deliberate: it lets the byte-emitting methods keep using `?` on
// `write!`/`write_all` unchanged, and new logical failures slot in as new
// variants without disturbing them. The `#[from] image::ImageError` bridge
// serves the same purpose for the image decode/encode path.
#[derive(Debug, Error)]
pub enum Error {
    /// A write to the runtime's output writer failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// [`pretty_print`](crate::vaxis::Vaxis::pretty_print) was called while in
    /// the alternate screen.
    #[error("pretty_print requires the primary screen")]
    NotInPrimaryScreen,

    /// A system-clipboard read was requested but clipboard access is not
    /// enabled in [`Options`](crate::vaxis::Options).
    #[error("system clipboard access is not enabled")]
    ClipboardDisabled,

    /// A terminal working-directory path that is not absolute. Carries the
    /// offending path.
    #[error("working directory must be an absolute path: {0}")]
    NotAbsolutePath(String),

    /// An image transmission was requested but the terminal does not support
    /// kitty graphics.
    #[error("terminal does not support kitty graphics")]
    NoGraphicsCapability,

    /// A local-image-path payload is too large to transmit in a single chunk.
    #[error("image path payload is too long to transmit")]
    PathTooLong,

    /// The image scaling math divides by the screen's cell dimensions, which
    /// must be non-zero. Raised by [`Image`](crate::image::Image) geometry when
    /// asked to scale against a screen with no cells.
    #[error("cannot compute image geometry for a zero-sized screen")]
    ZeroSizedScreen,

    /// Decoding or encoding an image failed.
    #[error(transparent)]
    Image(#[from] image::ImageError),
}
