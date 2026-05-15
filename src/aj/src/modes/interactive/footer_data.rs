//! Periodic data feeding the footer (current git branch, working
//! directory, model name, accumulated usage).
//!
//! The footer component reads its display state from a snapshot
//! refreshed on a `tokio::time::interval` plus on-demand whenever
//! the agent emits a relevant event (e.g. `TurnUsage`).
//!
//! Filled in by the "Interactive TUI" step in Phase 1.
