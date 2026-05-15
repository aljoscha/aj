//! Run modes for the `aj` binary.
//!
//! Per `docs/aj-next-plan.md` §4 the binary supports two modes
//! sharing the same agent core:
//!
//! - [`print`] — non-interactive; streams events to stdout (text
//!   or JSONL) and exits when the agent reports `AgentEnd`.
//! - [`interactive`] — full TUI built on [`aj-tui`].

pub mod interactive;
pub mod print;
