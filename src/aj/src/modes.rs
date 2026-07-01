//! Run modes for the `aj` binary.
//!
//! The binary supports two modes sharing the same agent core:
//!
//! - [`print`] — non-interactive; streams events to stdout (text
//!   or JSONL) and exits when the agent reports `AgentEnd`. Lives in
//!   [`aj_app`] (fully headless) and is re-exported here.
//! - [`interactive`] — full TUI built on [`aj-tui`].

pub mod interactive;

// Print mode and the background-task shutdown helper are
// frontend-agnostic and live in `aj_app`. Re-exported so the binary's
// `crate::modes::print` / `crate::modes::shutdown_background_tasks`
// paths keep resolving.
pub use aj_app::{print, shutdown_background_tasks};
