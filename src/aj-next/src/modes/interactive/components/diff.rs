//! Renders a `Diff`-flavoured tool result for `write_file` /
//! `edit_file` / `edit_file_multi`. Builds a unified diff from
//! the `before` / `after` byte pair on the fly and pipes it
//! through [`aj_tui`]'s syntax-highlighting layer.
//!
//! Filled in by the "Interactive TUI: layout slots, event pump,
//! components" step in Phase 1 of `docs/aj-next-plan.md`.
