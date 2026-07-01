//! Frontend-agnostic application logic shared by the `aj` (aj-tui) and
//! `aj-next` (vaxis) binaries.
//!
//! This crate holds everything a terminal frontend for the agent needs that is
//! not tied to a specific TUI backend: the CLI surface, model selection, the
//! session composition root, the turn driver, the theme palette, keybinding
//! data, and the chat model. The binaries supply the rendering.
//!
//! Invariant: `aj-app` must never depend on `aj-tui` or `vaxis`. That is what
//! keeps it shareable between the two frontends, and it is enforced in CI (see
//! `scripts/check-no-tui-dep.sh`).
