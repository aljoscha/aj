//! Frontend-agnostic application logic for the `aj` binary, kept independent of
//! the TUI backend.
//!
//! This crate holds everything a terminal frontend for the agent needs that is
//! not tied to a specific TUI backend: the CLI surface, model selection, the
//! session composition root, the turn driver, keybinding data, and the
//! non-interactive (print / subcommand) entry points. The binary supplies the
//! rendering.
//!
//! Invariant: `aj-app` must never depend on the `vaxis` TUI backend or directly
//! own an HTTP transport dependency. Those constraints keep the core independent
//! of its frontends. CI enforces them with `scripts/check-no-tui-dep.sh` and
//! `scripts/check-no-http-dep.sh`.

use std::collections::HashSet;
use std::time::Duration;

use aj_agent::TaskRegistry;
use aj_conf::Config;
use aj_session::{ProjectSessions, SessionMetadata};
use anyhow::Result;

pub mod actions;
pub mod auth;
pub mod chat;
pub mod cli;
pub mod client;
pub mod clipboard;
pub mod commands;
pub mod compact_sessions;
pub mod compaction;
pub mod composer;
pub mod directory;
pub mod export;
pub mod footer;
pub mod host;
pub mod keybindings;
pub mod markdown;
pub mod model;
pub mod notices;
pub mod oracle;
pub mod outbound;
pub mod print;
pub mod prompt_history;
pub mod recovery;
pub mod scripted;
pub mod session;
pub mod session_info;
pub mod session_preview;
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

/// What this build calls itself: the cargo version, then the git revision it
/// was built from, as in `0.1.0 (8cff8cc1abcd)`.
///
/// The revision ends in `-dirty` when the tree had uncommitted changes, and
/// is `unknown` for a build made outside a git checkout. Display only: the
/// cargo version does not move between builds, so the revision is the part
/// that tells two of them apart.
pub const VERSION: &str = env!("AJ_VERSION");

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

/// `aj list-sessions`: list existing sessions
/// for the current project, latest first.
///
/// Output: one row per session, as [`session_line`] formats it. Archived
/// sessions are marked, not dropped: a listing lists, and only the
/// interactive pickers filter. The underlying iteration and size formatting
/// live in [`ProjectSessions::list_sessions`] (`aj-session`);
/// this function resolves the store and prints what [`session_listing`]
/// answers.
pub fn handle_list_sessions() -> Result<()> {
    for line in session_listing(&current_project_persistence()?)? {
        println!("{line}");
    }
    Ok(())
}

/// Compose the global session store with the current project's legacy bucket.
/// Existing bucket identities remain stable across explicit JSONL migration.
pub fn current_project_persistence() -> Result<ProjectSessions> {
    project_persistence(Config::get_sessions_dir_path()?)
}

/// The current project's sessions in the shared database, with
/// `sessions_dir` as its legacy JSONL and writer-lock directory.
fn project_persistence(sessions_dir: std::path::PathBuf) -> Result<ProjectSessions> {
    Ok(ProjectSessions::for_project(
        Config::session_database_path()?,
        sessions_dir,
        Config::project_id()?,
    ))
}

/// Explicit, offline migration. This never opens an agent or runs recovery
/// actions. The session store owns validation, locking, and atomic import.
pub fn handle_migrate_session(session_id: Option<&str>, dry_run: bool) -> Result<()> {
    if let Some(id) = session_id {
        let store = project_persistence(Config::sessions_dir_path()?)?;
        let result = if dry_run {
            store.check_jsonl_migration(id)?
        } else {
            store.migrate_jsonl(id)?
        };
        println!("{id}: {result}. Source files were left unchanged.");
        return Ok(());
    }
    let (mut imported, mut already, mut planned, mut failed) = (0, 0, 0, 0);
    aj_session::migrate_all_jsonl(
        &Config::session_database_path()?,
        &Config::sessions_root_path()?,
        dry_run,
        |report| {
            let subject = match report.session_id {
                Some(id) => format!("{}/{id}", report.project_id),
                None => report.project_id,
            };
            match report.result {
                Ok(result) => {
                    match result {
                        aj_session::MigrationResult::Imported => imported += 1,
                        aj_session::MigrationResult::AlreadyImported => already += 1,
                        aj_session::MigrationResult::WouldImport => planned += 1,
                    }
                    println!("{subject}: {result}");
                }
                Err(error) => {
                    failed += 1;
                    println!("{subject}: failed: {error}");
                }
            }
        },
    )?;
    println!(
        "{imported} imported, {already} already imported, {planned} would import, {failed} failed. Source files were left unchanged."
    );
    anyhow::ensure!(failed == 0, "{failed} migration checks or imports failed");
    Ok(())
}

pub fn handle_export_session(session_id: &str, directory: &std::path::Path) -> Result<()> {
    let store = project_persistence(Config::sessions_dir_path()?)?;
    let path = store.export_jsonl(session_id, directory)?;
    println!("{session_id}: exported to {}", path.display());
    Ok(())
}

pub fn handle_import_session(file: &std::path::Path) -> Result<()> {
    let store = project_persistence(Config::sessions_dir_path()?)?;
    let id = store.import_jsonl(file)?;
    println!("{id}: imported into SQLite. The source file was left unchanged.");
    Ok(())
}

/// Report storage problems in the global session database, then the
/// commands it records as running. Exits nonzero when it finds a problem.
/// A task record is not one. Changes nothing.
pub fn handle_doctor() -> Result<()> {
    let store = project_persistence(Config::sessions_dir_path()?)?;
    let database = store.sqlite_path();
    let Some(problems) = store.check_database()? else {
        println!("{}: no session database yet", database.display());
        return Ok(());
    };
    for problem in &problems {
        println!("{problem}");
    }
    // A database with a problem may not be readable this far.
    match task_records_report(&store) {
        Ok(lines) => lines.iter().for_each(|line| println!("{line}")),
        Err(error) => println!("task records: could not be read: {error:#}"),
    }
    anyhow::ensure!(
        problems.is_empty(),
        "{}: {} problems found",
        database.display(),
        problems.len()
    );
    println!("{}: ok", database.display());
    Ok(())
}

/// The lines `doctor` prints for the commands `persistence`'s database
/// records as running, in every project and session: a count, then one
/// line a command with its session, the agent that started it, its task
/// id, its pid and start time, whether that process is still running, and
/// the command.
///
/// A process is the command's when it has the recorded pid, leads its
/// process group and started at the recorded time, which is the check a
/// host makes before it watches or signals one. A row whose process is not
/// running is one the next open of its session settles.
///
/// Separate from the printing so the store's answer is what a test reads.
pub fn task_records_report(persistence: &ProjectSessions) -> Result<Vec<String>> {
    /// How much of a command a line shows.
    const SHOWN: usize = 80;
    let commands = persistence.task_records()?;
    let mut lines = vec![format!("task records: {}", commands.len())];
    for (project, session, task) in commands {
        let agent = match task.owner {
            aj_agent::events::AgentId::Main => "main".to_string(),
            aj_agent::events::AgentId::Sub(n) => format!("sub {n}"),
        };
        let process = aj_tools::tools::bash::DetachedProcess {
            pid: task.pid,
            start_time: task.process_start,
            output_path: task.output_path,
            exit_path: task.exit_path,
        };
        let state = if process.alive() {
            "running"
        } else {
            "not running"
        };
        let command: String = task
            .command
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let command = match command.char_indices().nth(SHOWN) {
            Some((cut, _)) => format!("{}...", &command[..cut]),
            None => command,
        };
        lines.push(format!(
            "  {project}/{session}  {agent}  task #{}  pid {}  started {}  {state}  {command}",
            task.id,
            task.pid,
            task.started_at.format("%Y-%m-%dT%H:%M:%SZ"),
        ));
    }
    Ok(lines)
}

/// The lines `list-sessions` prints for `persistence`, latest first, or the
/// one line that says the store holds nothing.
///
/// Separate from the printing so the store's answer is what a test reads. The
/// archived set is enumerated once for the whole listing. An archive metadata
/// read failure costs the listing its markers, not its rows. The host directory
/// makes the same trade (see `host::store`).
fn session_listing(persistence: &ProjectSessions) -> Result<Vec<String>> {
    let sessions = persistence.list_sessions()?;
    if sessions.is_empty() {
        return Ok(vec!["No sessions found for this project.".to_string()]);
    }
    let archived: HashSet<String> = match persistence.enumerate_archived() {
        Ok(ids) => ids.into_iter().collect(),
        Err(err) => {
            tracing::warn!("could not read the store's archived sessions: {err}");
            HashSet::new()
        }
    };
    let mut lines = Vec::new();
    for session in &sessions {
        // A session that vanished since the listing has no size to show.
        let size = persistence.session_size(&session.session_id)?;
        let archived = archived.contains(&session.session_id);
        lines.push(session_line(session, size.unwrap_or(0), archived));
    }
    Ok(lines)
}

/// One `list-sessions` row: `<session_id> (modified: <utc-ts>, <size>)`, with a
/// trailing ` [archived]` when the session is archived. The size of the
/// session's history stands in for the session's length.
///
/// The marker trails the metadata so the ids stay left-aligned down the column
/// and every row that carries one carries it at the end.
fn session_line(session: &SessionMetadata, size_bytes: u64, archived: bool) -> String {
    let size = if size_bytes < 1024 {
        format!("{size_bytes}B")
    } else if size_bytes < 1024 * 1024 {
        format!("{}KB", size_bytes / 1024)
    } else {
        format!("{}MB", size_bytes / (1024 * 1024))
    };
    format!(
        "{} (modified: {}, {size}){}",
        session.session_id,
        session.modified_display(),
        if archived { " [archived]" } else { "" }
    )
}

/// `aj update-models`: refresh the on-disk model catalog at
/// `~/.aj/models.json` from `models.dev`. The model selector
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
    fn row(session_id: &str) -> SessionMetadata {
        let modified_at = DateTime::from_timestamp(1_700_000_000, 0).expect("a valid instant");
        SessionMetadata::new(session_id.to_string(), modified_at, 7)
    }

    #[test]
    fn an_archived_session_is_marked() {
        assert_eq!(
            session_line(&row("2024-01-01-00-00-00"), 2048, true),
            "2024-01-01-00-00-00 (modified: 2023-11-14 22:13:20 UTC, 2KB) [archived]",
            "an archived session printed with no marker, or with the rest of the row changed",
        );
    }

    #[test]
    fn an_unarchived_session_is_not_marked() {
        assert_eq!(
            session_line(&row("2024-01-01-00-00-00"), 2048, false),
            "2024-01-01-00-00-00 (modified: 2023-11-14 22:13:20 UTC, 2KB)",
            "an unarchived session printed a marker, or the row lost its id, time, or size",
        );
    }

    #[test]
    fn the_marker_is_all_that_archiving_adds() {
        let session = row("2024-02-03-04-05-06");
        assert_eq!(
            session_line(&session, 10, true),
            format!("{} [archived]", session_line(&session, 10, false)),
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
    fn store_with_two_sessions() -> (TempDir, ProjectSessions, String, String) {
        use aj_agent::message::AgentMessage;
        use aj_models::types::{Message, UserMessage};
        use aj_session::{LogEntryKind, SessionLog, ThreadKind};

        let dir = TempDir::new().expect("tempdir");
        let persistence = ProjectSessions::new(dir.path().join("sessions"));
        let kept = {
            let mut log = SessionLog::create(&persistence).expect("create log");
            // The listing reads each session's first line to tell the current
            // format from the old one, so a log with nothing written is not a
            // session it lists.
            log.set_system_prompt("system".to_string())
                .expect("system prompt");
            let root = log.system_prompt_id().cloned().expect("system prompt id");
            log.append(
                Some(root),
                ThreadKind::Main,
                None,
                LogEntryKind::Message {
                    message: AgentMessage::wire(Message::User(UserMessage::text("a prompt"))),
                },
            )
            .expect("a prompt");
            log.session_id().to_string()
        };
        let put_away = "2024-02-03-04-05-06-007".to_string();
        // A JSONL copy of the first session under another id.
        let copy = persistence
            .export_jsonl(&kept, &dir.path().join("exported"))
            .expect("export the first session");
        let second = persistence.sessions_dir().join(format!("{put_away}.jsonl"));
        std::fs::copy(copy, second).expect("a second session in the store");
        persistence
            .write_archived(&put_away, true)
            .expect("archive the second session");
        (dir, persistence, kept, put_away)
    }

    /// The marker follows the store's archive metadata, and every session is
    /// listed either way.
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
            "an unarchived session printed as archived: {}",
            line_for(&kept),
        );
    }
}
