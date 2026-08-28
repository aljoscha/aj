//! Frontend-agnostic application logic for the `aj` binary, kept independent of
//! the TUI backend.
//!
//! This crate holds everything a terminal frontend for the agent needs that is
//! not tied to a specific TUI backend: the CLI surface, model selection, the
//! session composition root, the turn driver, keybinding data, and the
//! non-interactive (print / subcommand) entry points. The binary supplies the
//! rendering.
//!
//! Invariant: `aj-app` must never depend on the `vaxis` TUI backend. That is
//! what keeps the core independent of the frontend, and it is enforced in CI
//! (see `scripts/check-no-tui-dep.sh`).

use std::collections::HashSet;
use std::time::Duration;

use aj_agent::TaskRegistry;
use aj_conf::Config;
use aj_session::{ConversationPersistence, SessionMetadata};
use anyhow::Result;

pub mod actions;
pub mod auth;
pub mod chat;
pub mod cli;
pub mod client;
pub mod clipboard;
pub mod commands;
pub mod compaction;
pub mod diff;
pub mod directory;
pub mod export;
pub mod footer;
pub mod host;
pub mod keybindings;
pub mod markdown;
pub mod model;
pub mod notices;
pub mod print;
pub mod scripted;
pub mod session;
pub mod session_info;
pub mod session_setup;
pub mod settings;
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
const TASK_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Kill the background-task tree and await driver quiescence with a bounded grace.
pub async fn shutdown_background_tasks(registry: &TaskRegistry) {
    if !shutdown_background_tasks_quietly(registry).await {
        tracing::warn!(
            phase = "background task quiesce",
            "background tasks still running after the shutdown grace; forced abortable drivers and stopped waiting"
        );
    }
}

/// Kill the background-task tree and report whether every driver quiesced.
pub(crate) async fn shutdown_background_tasks_quietly(registry: &TaskRegistry) -> bool {
    registry.shutdown();
    let graceful = registry.quiesce(TASK_SHUTDOWN_GRACE).await;
    if !graceful {
        registry.abort_drivers();
    }
    graceful
}

/// Cancel background work and retain ownership until every detached driver has
/// returned.
///
/// Session supervisors use this before releasing an advisory lock. Unlike the
/// frontend helper above, it has no second deadline after the forced cutoff:
/// an uncooperative process keeps the supervisor and lock alive rather than
/// becoming work owned by nobody.
pub(crate) async fn shutdown_background_tasks_owned(registry: &TaskRegistry) -> bool {
    let graceful = shutdown_background_tasks_quietly(registry).await;
    if !graceful {
        registry.wait_for_quiescence().await;
    }
    graceful
}

/// `aj list-sessions`: list existing conversation sessions
/// for the current project, latest first.
///
/// Output: one row per session, as [`session_line`] formats it. Archived
/// sessions are marked, not dropped: a listing lists, and only the
/// interactive pickers filter. The underlying iteration,
/// pre-refactor-format filtering, and size formatting all live
/// in [`ConversationPersistence::list_sessions`] (`aj-session`);
/// this function resolves the store and prints what [`session_listing`]
/// answers.
pub fn handle_list_sessions() -> Result<()> {
    let sessions_dir = Config::get_sessions_dir_path()?;
    for line in session_listing(&ConversationPersistence::new(sessions_dir))? {
        println!("{line}");
    }
    Ok(())
}

/// The lines `list-sessions` prints for `persistence`, latest first, or the
/// one line that says the store holds nothing.
///
/// Separate from the printing so the store's answer is what a test reads. The
/// archived set is one directory read for the whole listing, since the
/// sidecar's existence is the bit. A `meta/` directory that cannot be read
/// costs the rows their markers and not the caller its listing: the bit is
/// display metadata.
fn session_listing(persistence: &ConversationPersistence) -> Result<Vec<String>> {
    let sessions = persistence.list_sessions()?;
    if sessions.is_empty() {
        return Ok(vec![
            "No conversation sessions found for this project.".to_string(),
        ]);
    }
    let archived: HashSet<String> = match persistence.enumerate_archived() {
        Ok(sidecars) => sidecars
            .into_iter()
            .map(|sidecar| sidecar.session_id)
            .collect(),
        Err(err) => {
            tracing::warn!("could not read the store's archived sidecars: {err}");
            HashSet::new()
        }
    };
    Ok(sessions
        .iter()
        .map(|session| session_line(session, archived.contains(&session.session_id)))
        .collect())
}

/// One `list-sessions` row: `<session_id> (modified: <utc-ts>, <size>)`, with a
/// trailing ` [archived]` when the session is archived.
///
/// The marker trails the metadata so the ids stay left-aligned down the column
/// and every row that carries one carries it at the end.
fn session_line(session: &SessionMetadata, archived: bool) -> String {
    format!(
        "{} (modified: {}, {}){}",
        session.session_id,
        session.modified_display(),
        session.size_display(),
        if archived { " [archived]" } else { "" }
    )
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

#[cfg(test)]
mod tests {
    use chrono::DateTime;
    use tempfile::TempDir;

    use super::*;

    /// A row modified at a fixed instant, so a test can pin the whole line and
    /// not only the marker.
    fn row(session_id: &str, size_bytes: u64) -> SessionMetadata {
        let modified_at = DateTime::from_timestamp(1_700_000_000, 0).expect("a valid instant");
        SessionMetadata::new(session_id.to_string(), modified_at, size_bytes)
    }

    #[test]
    fn an_archived_session_is_marked() {
        assert_eq!(
            session_line(&row("2024-01-01-00-00-00", 2048), true),
            "2024-01-01-00-00-00 (modified: 2023-11-14 22:13:20 UTC, 2KB) [archived]",
            "an archived session printed with no marker, or with the rest of the row changed",
        );
    }

    #[test]
    fn an_unarchived_session_is_not_marked() {
        assert_eq!(
            session_line(&row("2024-01-01-00-00-00", 2048), false),
            "2024-01-01-00-00-00 (modified: 2023-11-14 22:13:20 UTC, 2KB)",
            "an unarchived session printed a marker, or the row lost its id, time, or size",
        );
    }

    #[test]
    fn the_marker_is_all_that_archiving_adds() {
        let session = row("2024-02-03-04-05-06", 10);
        assert_eq!(
            session_line(&session, true),
            format!("{} [archived]", session_line(&session, false)),
            "archiving a session changed more of its row than the trailing marker",
        );
    }

    /// Two sessions in one store, one of them archived: a real log the store
    /// minted, and a copy of it under a second id.
    ///
    /// Copied rather than minted twice because ids carry the time of day, so a
    /// second mint would either collide or cost the test a wall-clock second.
    /// What the listing reads of a session is its first line and its file
    /// stat, and the copy answers both.
    fn store_with_two_sessions() -> (TempDir, ConversationPersistence, String, String) {
        use aj_agent::message::AgentMessage;
        use aj_models::types::{Message, UserMessage};
        use aj_session::{ConversationEntryKind, ConversationLog, ThreadKind};

        let dir = TempDir::new().expect("tempdir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let kept = {
            let mut log = ConversationLog::create(&persistence).expect("create log");
            // The listing reads each session's first line to tell the current
            // format from the old one, so a log with nothing written is not a
            // session it lists.
            log.set_system_prompt("system".to_string())
                .expect("system prompt");
            let root = log.system_prompt_id().cloned().expect("system prompt id");
            log.append(
                Some(root),
                ThreadKind::User,
                None,
                ConversationEntryKind::Message {
                    message: AgentMessage::wire(Message::User(UserMessage::text("a prompt"))),
                },
            )
            .expect("a prompt");
            log.session_id().to_string()
        };
        let put_away = "2024-02-03-04-05-06-007".to_string();
        let log_path = |id: &str| persistence.sessions_dir().join(format!("{id}.jsonl"));
        std::fs::copy(log_path(&kept), log_path(&put_away)).expect("a second session in the store");
        persistence
            .write_archived(&put_away, true)
            .expect("archive the second session");
        (dir, persistence, kept, put_away)
    }

    /// The listing reads the store's own sidecars: the marker follows the
    /// session that has one, and every session is listed either way.
    #[test]
    fn the_listing_marks_the_sessions_the_store_says_are_archived() {
        let (_dir, persistence, kept, put_away) = store_with_two_sessions();
        let lines = session_listing(&persistence).expect("the listing");
        assert_eq!(lines.len(), 2, "a listing lists every session: {lines:?}");
        let line_for = |id: &str| -> String {
            lines
                .iter()
                .find(|line| line.starts_with(id))
                .unwrap_or_else(|| panic!("{id} is not in the listing: {lines:?}"))
                .clone()
        };
        assert!(
            line_for(&put_away).ends_with(" [archived]"),
            "the archived session printed with no marker: {}",
            line_for(&put_away),
        );
        assert!(
            !line_for(&kept).contains("[archived]"),
            "a session with no sidecar printed as archived: {}",
            line_for(&kept),
        );
    }
}
