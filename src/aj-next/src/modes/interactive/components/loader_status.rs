//! Renders the spinner / loader status line shown while the
//! agent is mid-inference or mid-tool-call. Drives off the
//! [`crate::modes::interactive::footer_data`] snapshot plus the
//! agent's `is_streaming` / `pending_tool_calls` state.
//!
//! Filled in by the "Interactive TUI: layout slots, event pump,
//! components" step in Phase 1 of `docs/aj-next-plan.md`.
