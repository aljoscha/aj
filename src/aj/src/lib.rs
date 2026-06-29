//! `aj` — event-driven core + TUI binary.
//!
//! This crate hosts the `aj` binary built on top of `aj-agent`'s
//! typed [`AgentEvent`] stream and the in-process [`aj-tui`]
//! framework. The same crate also provides a non-interactive print
//! mode so the agent can be scripted or embedded in a parent process.
//!
//! Structure:
//!
//! - [`cli`] — argument parsing and `@file` expansion.
//! - [`config`] — keybindings, theme, command catalog.
//! - [`modes`] — `print` (text/JSONL) and `interactive` (TUI).
//! - [`persistence`] — thin wrapper that builds the
//!   `aj-session` persistence listener for either mode.
//!
//! [`AgentEvent`]: aj_agent::events::AgentEvent

// The default system prompt is embedded at compile time so `aj` ships
// as a single self-contained binary. The file lives next to
// `Cargo.toml` in this crate. At runtime, a `~/.agents/SYSTEM_PROMPT.md`
// (or `~/.claude/SYSTEM_PROMPT.md`) override file replaces it; see
// `AgentEnv` in `aj-conf`.
pub const SYSTEM_PROMPT: &str = include_str!("../SYSTEM_PROMPT.md");

pub mod auth;
pub mod cli;
pub mod clipboard;
pub mod compaction;
pub mod config;
pub mod export;
pub mod model;
pub mod modes;
pub mod scripted;
pub mod session_setup;
pub mod system_prompt;
pub mod tmux_notice;
pub mod turn;
pub mod usage;
