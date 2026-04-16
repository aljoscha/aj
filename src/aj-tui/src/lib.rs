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
