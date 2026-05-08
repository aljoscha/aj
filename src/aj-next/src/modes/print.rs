//! Non-interactive print mode.
//!
//! Per `docs/aj-next-plan.md` §4.2 the same `aj-next` binary can
//! run without a TUI: it subscribes to the agent's event bus and
//! writes plain text (or JSONL with `--format json`) to stdout,
//! exiting when the agent reports `AgentEnd`. Same code path lets
//! callers script the agent or embed it in a parent process.
//!
//! Filled in by the "Print mode (text, JSONL)" step in Phase 1.

use anyhow::{Result, bail};

use crate::cli::args::Args;

/// Drive a single print-mode run from `args`.
///
/// The scaffold short-circuits with a clear error so users (and
/// the test suite) know the mode isn't wired up yet. The next
/// Phase 1 step replaces this body with a real implementation
/// that awaits events from the agent's bus, hence the `async`
/// signature on what is currently a synchronous stub.
#[allow(clippy::unused_async)]
pub async fn run(_args: Args) -> Result<()> {
    bail!("aj-next print mode is not yet implemented");
}
