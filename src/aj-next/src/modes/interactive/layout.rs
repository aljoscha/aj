//! Layout slots for the interactive mode.
//!
//! Per `docs/aj-next-plan.md` §4 the TUI lays out named regions
//! (header, scrollback, footer, editor, overlay) and components
//! attach to a slot rather than to absolute coordinates. The
//! event pump can then update component state without knowing
//! anything about geometry.
//!
//! Filled in by the "Interactive TUI: layout slots, event pump,
//! components" step in Phase 1.
