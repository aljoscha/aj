//! Naming alias for [`super::virtual_terminal::VirtualTerminal`] used by
//! tests that specifically care about the raw escape sequences written to
//! the terminal.
//!
//! The underlying `VirtualTerminal` already records every `write()`; tests
//! just need to call [`VirtualTerminal::writes_joined`] or
//! [`VirtualTerminal::clear_writes`]. This alias exists so a test's type
//! signature makes its intent obvious at a glance.

use super::virtual_terminal::VirtualTerminal;

/// A `VirtualTerminal` whose `write` log is the point of interest for the
/// test (e.g. asserting "no `\x1b[2J` was emitted on resize").
///
/// This is a zero-cost re-export; see the module docs for the rationale.
pub type LoggingVirtualTerminal = VirtualTerminal;
