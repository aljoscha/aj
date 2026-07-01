//! `aj` — event-driven core + TUI binary.
//!
//! This crate hosts the `aj` binary built on top of `aj-agent`'s
//! typed [`AgentEvent`] stream and the in-process [`aj-tui`]
//! framework. The frontend-agnostic application logic (CLI surface,
//! model selection, session setup, turn driver, print mode) lives in
//! [`aj_app`] and is re-exported here so existing `crate::...` paths
//! keep resolving.
//!
//! Structure:
//!
//! - [`cli`] — argument parsing and `@file` expansion (from `aj_app`).
//! - [`config`] — keybindings, theme, command catalog.
//! - [`modes`] — `print` (text/JSONL) and `interactive` (TUI).
//!
//! [`AgentEvent`]: aj_agent::events::AgentEvent

pub use aj_app::{auth, cli, clipboard, export, model, session, session_setup, turn, usage};

pub mod config;
pub mod modes;
pub mod tmux_notice;
