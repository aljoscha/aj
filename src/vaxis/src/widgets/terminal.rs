//! An embedded terminal emulator: the OS-free core.
//!
//! This is the self-contained half of the emulator, the part that has no
//! dependency on a PTY, a child process, or any OS facility:
//!
//! - [`ansi`]: the `C0` control table, the `CSI` value, and the parameter
//!   iterator shared by the SGR machine and CSI dispatch.
//! - [`parser`]: the streaming VT parser that decodes a child's output byte
//!   stream into [`parser::Event`]s. It is the inverse of the crate's
//!   top-level input [`crate::parser`]: that one turns terminal input into
//!   application events, this one turns a program's output into screen
//!   operations.
//! - [`screen`]: the emulator's own grid, with per-cell owned graphemes and
//!   the full SGR attribute machine. Distinct from the crate's top-level
//!   [`crate::screen`].
//! - [`key`]: encoding a [`crate::key::Key`] back into the bytes a child
//!   expects on its input.
//!
//! NOTE: The PTY, the child-process `Command`, and the `Terminal` orchestrator
//! (reader thread, triple-buffered screens, event queue) are a separate
//! follow-up. They carry all the OS-specific machinery and are Linux-first.

pub mod ansi;
pub mod key;
pub mod parser;
pub mod screen;
