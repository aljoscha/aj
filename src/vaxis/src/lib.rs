//! A faithful Rust port of the Zig terminal UI library libvaxis.
//!
//! The module tree mirrors libvaxis's single-module layout: a cell and style
//! value model ([`cell`]), the front and back screen buffers ([`screen`],
//! [`internal_screen`]) viewed through clipped [`window`] handles, a pure
//! input [`parser`] feeding [`key`], [`mouse`], and [`event`] value types, a
//! thread-safe [`queue`] and threaded [`event_loop`], the [`tty`] OS boundary,
//! the [`vaxis`] runtime and renderer, kitty graphics ([`image`]), the two
//! widget layers ([`widgets`], [`vxfw`]), and the widget-free text-editing
//! primitives ([`text`]) the widgets build on.
//!
//! Shared leaf types that everything else depends on (such as [`Winsize`])
//! live here at the crate root to break the import cycles upstream expresses
//! within its single module.

pub mod cell;
pub mod ctlseqs;
pub mod error;
pub mod event;
pub mod event_loop;
pub mod fuzzy;
pub mod grapheme_cache;
pub mod gwidth;
pub mod image;
pub mod internal_screen;
pub mod key;
pub mod mouse;
pub mod parser;
pub mod queue;
pub mod screen;
pub mod text;
pub mod tty;
pub mod unicode;
pub mod vaxis;
pub mod vxfw;
pub mod widgets;
pub mod window;

pub use crate::error::Error;

/// The `#[derive(TableRow)]` macro. Lives in the macro namespace, so importing
/// it alongside the [`TableRow`](crate::widgets::table::TableRow) trait via a
/// single `use vaxis::TableRow` brings both into scope.
pub use vaxis_derive::TableRow;

/// The `TableRow` trait, re-exported at the crate root next to its derive
/// macro. Trait and macro share a name but live in different namespaces.
pub use crate::widgets::table::TableRow;

/// The vaxis logo, in PixelCode block glyphs. Four lines joined by `\n`, 28
/// columns wide, with no trailing newline.
///
/// Named `LOGO` (upstream `logo`) to satisfy the const-naming lint.
pub const LOGO: &str = concat!(
    "▄   ▄  ▄▄▄  ▄   ▄ ▄▄▄  ▄▄▄\n",
    "█   █ █▄▄▄█ ▀▄ ▄▀  █  █   ▀\n",
    "▀▄ ▄▀ █   █  ▄▀▄   █   ▀▀▀▄\n",
    " ▀▄▀  █   █ █   █ ▄█▄ ▀▄▄▄▀",
);

/// Terminal window size in character cells and pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub x_pixel: u16,
    pub y_pixel: u16,
}

/// Resets terminal state using the global tty. Use only to recover during a
/// panic. See [`tty::recover`].
#[cfg(unix)]
pub use crate::tty::recover;
