//! `aj-next` — event-driven core + new TUI binary.
//!
//! Per `docs/aj-next-plan.md` Phase 1 (§4), this crate hosts a new
//! `aj` binary built on top of `aj-agent`'s typed [`AgentEvent`]
//! stream and the in-process [`aj-tui`] framework. The same crate
//! also provides a non-interactive print mode (§4.2) so the agent
//! can be scripted or embedded in a parent process.
//!
//! The legacy `aj` binary keeps working in parallel until the
//! Phase 2 cutover (§5), at which point this crate is renamed to
//! `aj` and the old crate is deleted.
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

// The system prompt is a single source of truth shared with the
// legacy `aj` binary during the Phase 0 → Phase 2 transition. The
// file moves into this crate at cutover (§5); for now we read it
// in place via a relative `include_str!` so both binaries embed
// the exact same bytes.
pub const SYSTEM_PROMPT: &str = include_str!("../../aj/SYSTEM_PROMPT.md");

pub mod cli;
pub mod config;
pub mod modes;
pub mod persistence;
