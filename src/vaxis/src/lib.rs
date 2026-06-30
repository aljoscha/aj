//! A faithful Rust port of the Zig terminal UI library libvaxis.
//!
//! The module tree mirrors libvaxis's single-module layout: a cell and style
//! value model ([`cell`]), the front and back screen buffers ([`screen`],
//! [`internal_screen`]) viewed through clipped [`window`] handles, a pure
//! input [`parser`] feeding [`key`], [`mouse`], and [`event`] value types, a
//! thread-safe [`queue`] and threaded [`event_loop`], the [`tty`] OS boundary,
//! the [`vaxis`] runtime and renderer, kitty graphics ([`image`]), and the two
//! widget layers ([`widgets`], [`vxfw`]).
//!
//! Shared leaf types that everything else depends on (such as [`Winsize`])
//! live here at the crate root to break the import cycles upstream expresses
//! within its single module.

pub mod cell;
pub mod ctlseqs;
pub mod event;
pub mod event_loop;
pub mod grapheme_cache;
pub mod gwidth;
pub mod image;
pub mod internal_screen;
pub mod key;
pub mod mouse;
pub mod parser;
pub mod queue;
pub mod screen;
pub mod tty;
pub mod unicode;
pub mod vaxis;
pub mod vxfw;
pub mod widgets;
pub mod window;

/// Terminal window size in character cells and pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Winsize {
    pub rows: u16,
    pub cols: u16,
    pub x_pixel: u16,
    pub y_pixel: u16,
}
