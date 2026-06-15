//! Keymap helpers specific to the interactive mode.
//!
//! Wraps [`aj_tui::keybindings`] with `aj`-level bindings:
//! double-Esc cancel + clear-queues, `Ctrl+O` to open the command
//! palette, `Ctrl+C` to interrupt a running turn, etc.
//!
//! Filled in by the "Interactive TUI" step in Phase 1.
