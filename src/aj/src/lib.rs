//! `aj` — event-driven core + TUI binary.
//!
//! Per `docs/aj-next-plan.md` Phase 1 (§4), this crate hosts the
//! `aj` binary built on top of `aj-agent`'s typed [`AgentEvent`]
//! stream and the in-process [`aj-tui`] framework. The same crate
//! also provides a non-interactive print mode (§4.2) so the agent
//! can be scripted or embedded in a parent process.
//!
//! Structure mirrors the plan's §4 layout:
//!
//! - [`cli`] — argument parsing and `@file` expansion.
//! - [`config`] — keybindings, theme, slash-command registry.
//! - [`modes`] — `print` (text/JSONL) and `interactive` (TUI).
//! - [`persistence`] — thin wrapper that builds the
//!   `aj-session` persistence listener for either mode.
//!
//! [`AgentEvent`]: aj_agent::events::AgentEvent

// The system prompt is embedded at compile time so `aj` ships as a
// single self-contained binary. The file lives next to `Cargo.toml`
// in this crate.
pub const SYSTEM_PROMPT: &str = include_str!("../SYSTEM_PROMPT.md");

pub mod cli;
pub mod config;
pub mod modes;
pub mod persistence;
pub mod scripted;
