//! A minimal, component-based terminal UI framework.
//!
//! This framework provides differential line-based rendering, an overlay compositor,
//! a component trait, and a set of built-in widgets suitable for building interactive
//! terminal applications.
//!
//! # Design principle: in-process only
//!
//! The crate intentionally does not shell out to external binaries at
//! runtime. Filesystem traversal for [`autocomplete`] goes through the
//! [`ignore`](https://docs.rs/ignore) crate (the library backing both
//! `ripgrep` and `fd`) rather than spawning `fd`, terminal input goes
//! through `crossterm` rather than a shell, and so on. This keeps the
//! crate portable, deterministic in tests, and free of optional-tool
//! probes. If a future feature looks like it needs an external binary,
//! reach for a Rust crate first.
//!
//! ## Why no stdin buffering layer
//!
//! Crossterm's [`EventStream`] frames raw stdin bytes into typed
//! [`InputEvent`] values: its internal parser buffers partial CSI / OSC
//! / DCS sequences and only emits an `Event` once a full sequence (or
//! printable codepoint) has arrived. By the time bytes reach an aj
//! component they are already typed events, so no separate reassembly
//! layer is needed.
//!
//! [`EventStream`]: https://docs.rs/crossterm/latest/crossterm/event/struct.EventStream.html
//! [`InputEvent`]: crate::keys::InputEvent
//!
//! ## Inline image rendering
//!
//! Inline images render through [`crate::image_protocol`] (Kitty
//! graphics + iTerm2 OSC 1337 encoders, multi-row layout helpers)
//! and [`crate::components::image::Image`]. Cell pixel size is
//! sourced from [`crate::terminal::Terminal::cell_pixel_size`]
//! when the host terminal reports it. The differential renderer
//! in [`crate::tui`] is aware of image rows: it skips width
//! validation and the per-line SGR/OSC reset on rows that carry
//! a protocol escape, tracks placed Kitty image IDs across
//! frames, and emits delete-by-id escapes before redrawing any
//! row that previously held a placement (Kitty doesn't replace
//! placements by overwriting cells).
//!
//! # Testing
//!
//! Integration tests live under `tests/` and share a support layer at
//! `tests/support/` — a headless `VirtualTerminal` backed by a VT parser,
//! env-var RAII guards, theme factories, and small component fixtures.
//! See `tests/support/README.md` for the full shape.

pub mod ansi;
pub mod autocomplete;
pub mod capabilities;
pub mod component;
pub mod components;
pub mod container;
pub mod editor_component;
pub mod fuzzy;
pub mod image_protocol;
pub mod keybindings;
pub mod keys;
pub mod kill_ring;
pub mod style;
pub mod terminal;
pub mod tui;
pub mod undo_stack;
pub mod word_boundary;
pub mod word_wrap;

pub use editor_component::EditorComponent;
