//! Run modes for the `aj` binary.
//!
//! The binary supports two modes sharing the same agent core:
//!
//! - [`print`] — non-interactive; streams events to stdout (text
//!   or JSONL) and exits when the agent reports `AgentEnd`.
//! - [`interactive`] — full TUI built on [`aj-tui`].

use std::time::Duration;

use aj_agent::TaskRegistry;

pub mod interactive;
pub mod print;

/// Bounded grace both modes give task drivers to observe the root
/// cancel, kill their process groups, and reap before teardown
/// proceeds. Drivers respond promptly (SIGKILL + reap, or a cancelled
/// child run), so this only guards against a wedged driver.
const TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Kill the background-task tree and await driver quiescence with a
/// bounded grace, so process groups are reliably killed and reaped
/// before the caller tears the rest of the world down.
pub(crate) async fn shutdown_background_tasks(registry: &TaskRegistry) {
    registry.shutdown();
    if !registry.quiesce(TASK_SHUTDOWN_GRACE).await {
        tracing::warn!("background tasks still running after the shutdown grace; proceeding");
    }
}
