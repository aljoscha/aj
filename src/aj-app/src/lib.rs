//! Frontend-agnostic application logic shared by the `aj` (aj-tui) and
//! `aj-next` (vaxis) binaries.
//!
//! This crate holds everything a terminal frontend for the agent needs that is
//! not tied to a specific TUI backend: the CLI surface, model selection, the
//! session composition root, the turn driver, keybinding data, and the
//! non-interactive (print / subcommand) entry points. The binaries supply the
//! rendering.
//!
//! Invariant: `aj-app` must never depend on `aj-tui` or `vaxis`. That is what
//! keeps it shareable between the two frontends, and it is enforced in CI (see
//! `scripts/check-no-tui-dep.sh`).

use std::time::Duration;

use aj_agent::TaskRegistry;
use aj_conf::Config;
use aj_session::ConversationPersistence;
use anyhow::Result;

pub mod auth;
pub mod cli;
pub mod clipboard;
pub mod commands;
pub mod compaction;
pub mod export;
pub mod footer;
pub mod keybindings;
pub mod model;
pub mod print;
pub mod scripted;
pub mod session;
pub mod session_setup;
pub mod shutdown;
pub mod system_prompt;
pub mod theme;
pub mod tmux;
pub mod turn;
pub mod usage;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use system_prompt::SYSTEM_PROMPT;

/// Bounded grace both modes give task drivers to observe the root
/// cancel, kill their process groups, and reap before teardown
/// proceeds. Drivers respond promptly (SIGKILL + reap, or a cancelled
/// child run), so this only guards against a wedged driver.
const TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Kill the background-task tree and await driver quiescence with a
/// bounded grace, so process groups are reliably killed and reaped
/// before the caller tears the rest of the world down.
pub async fn shutdown_background_tasks(registry: &TaskRegistry) {
    registry.shutdown();
    if !registry.quiesce(TASK_SHUTDOWN_GRACE).await {
        tracing::warn!("background tasks still running after the shutdown grace; proceeding");
    }
}

/// `aj list-sessions`: list existing conversation sessions
/// for the current project, latest first.
///
/// Output: one row per session, formatted as `<session_id>
/// (modified: <utc-ts>, <size>)`. The underlying iteration,
/// pre-refactor-format filtering, and size formatting all live
/// in [`ConversationPersistence::list_sessions`] (`aj-session`);
/// this function is a thin presentation wrapper.
pub fn handle_list_sessions() -> Result<()> {
    let sessions_dir = Config::get_sessions_dir_path()?;
    let conversation_persistence = ConversationPersistence::new(sessions_dir);
    let sessions = conversation_persistence.list_sessions()?;

    if sessions.is_empty() {
        println!("No conversation sessions found for this project.");
        return Ok(());
    }

    for session in sessions {
        println!(
            "{} (modified: {}, {})",
            session.session_id, session.modified, session.size_display
        );
    }

    Ok(())
}

/// `aj update-models`: refresh the on-disk model catalog at
/// `~/.aj/models.json` from `models.dev`. The `/model` selector
/// overlay reads that catalog at startup, so running this command
/// is how users surface freshly-released models to the picker
/// without restarting from a different catalog source.
///
/// The output is a one-line summary (added / removed /
/// price-changes counts plus total + destination path) suitable
/// for scripting.
pub async fn handle_update_models_command() -> Result<()> {
    let summary = aj_models::refresh::refresh_user_cache().await?;
    println!("{}", summary.one_line());
    Ok(())
}
