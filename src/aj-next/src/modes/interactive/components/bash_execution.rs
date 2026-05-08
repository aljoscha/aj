//! Renders a `bash`-tool execution: live stdout / stderr tails,
//! exit code, and the truncation / spill-file marker. Subscribes
//! to `ToolExecutionUpdate` events for the throttled
//! `ToolDetails::Bash` snapshots emitted while the child runs.
//!
//! Filled in by the "Interactive TUI: layout slots, event pump,
//! components" step in Phase 1 of `docs/aj-next-plan.md`.
