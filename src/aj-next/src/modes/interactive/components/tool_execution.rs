//! Renders a generic tool-execution block (`Text`,
//! `SubAgentReport`, `Todos`, `Json`) for the interactive mode.
//! `Diff`- and `Bash`-flavoured executions get their own
//! components ([`super::diff`], [`super::bash_execution`]) so
//! they can render specialised UI.
//!
//! Filled in by the "Interactive TUI: layout slots, event pump,
//! components" step in Phase 1 of `docs/aj-next-plan.md`.
