//! Keybinding registry for the interactive mode.
//!
//! The interactive editor and overlays consume an
//! [`aj_tui::keybindings`]-style registry; this module wraps that
//! registry with `aj-next` defaults (Emacs-style editor bindings,
//! `Ctrl+O` to expand sub-agent transcripts, double-Esc to cancel
//! and clear queues, etc.) plus user overrides loaded from the
//! shared `aj-conf` config file.
//!
//! Filled in by the "Selectors and theming" step in Phase 1.
