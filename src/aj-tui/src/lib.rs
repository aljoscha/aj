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
//! ## Why no image rendering yet
//!
//! Inline image rendering (encoder, terminal fallback path, cell-dimension
//! probing, and an `Image` component) is intentionally deferred until aj
//! has a concrete consumer for it. The capability-detection layer
//! ([`crate::capabilities`]) is in place regardless: components consult
//! it to gate optional escape sequences (OSC 8 hyperlinks, true color,
//! image-protocol probes).
//!
//! Knock-on absence: the cell-size protocol response (`\x1b[6;<h>;<w>t`)
//! is not intercepted. `Tui::handle_input_after_listeners` carries an
//! inline note explaining the deliberate gap.
//!
//! Revisit when an aj surface needs to render images: add an encoder /
//! fallback / cell-dim probe module, an `Image` component, and re-enable
//! the cell-size response branch in the `Tui` input dispatcher.
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
