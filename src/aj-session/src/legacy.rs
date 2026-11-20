//! Session files that were never imported.
//!
//! Before the database, a session was a JSONL file in its project's
//! sessions directory, with its label and archive marker as sidecars
//! beside it (the layout of [`crate::jsonl::Directory`]). Such a file is
//! still a session until it is imported. It is listed, previewed, searched
//! and labelled where it lies, and resuming it imports it. From then on
//! the database is the session and the file is never read again. Nothing
//! here modifies a session file.
//!
//! Everything that touches those files is in this module. The rest of the
//! crate deals with the database, and asks here only about a session the
//! database does not hold. Whether it holds one is the caller's knowledge:
//! no function here opens the database to find out, except the imports,
//! which write to it.
//!
//! Removing this module and its callers removes the support. JSONL as the
//! export and import format is [`crate::jsonl`] and stays.

use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

use crate::jsonl::{ARCHIVED_SIDECAR, Directory, TAG_SIDECAR, TailRepair};
use crate::log::ThreadKind;
use crate::persistence::{
    PreviewAccumulator, SessionMetadata, SessionPreview, parse_session_id_created_at,
};
use crate::prompt_history::RecordedPrompt;
use crate::sqlite::{SessionAnnotations, SqliteStore};
use crate::{LogEntry, ProjectSessions, SessionError, SessionLock};

fn directory(store: &ProjectSessions) -> Directory<'_> {
    Directory(store.sessions_dir())
}

/// Where the file of session `session_id` is, whether or not it exists.
pub(crate) fn session_path(store: &ProjectSessions, session_id: &str) -> PathBuf {
    directory(store).session_path(session_id)
}

/// Whether `sessions_dir` has a session file under `session_id`, which
/// makes the id taken.
pub(crate) fn takes_id(sessions_dir: &Path, session_id: &str) -> bool {
    Directory(sessions_dir).session_path(session_id).exists()
}

/// Whether the project has a session file under `session_id`.
pub(crate) fn holds(store: &ProjectSessions, session_id: &str) -> std::io::Result<bool> {
    session_path(store, session_id).try_exists()
}

/// Every session file of the project whose stem is a session id, with the
/// facts a `stat` yields: the modification time, and the length as the
/// revision. Contents are not opened. The caller leaves out the sessions
/// the database holds.
///
/// A stem the grammar rejects (see [`crate::id`]) is skipped: the store
/// refuses that id at every later lookup. A file that vanishes or turns
/// unreadable between the directory read and its `stat` is skipped too: a
/// listing must not break over one file.
pub(crate) fn sessions(store: &ProjectSessions) -> Result<Vec<SessionMetadata>, SessionError> {
    let mut sessions = Vec::new();
    for (session_id, metadata) in store.session_files(store.sessions_dir(), "jsonl")? {
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        sessions.push(SessionMetadata::new(
            session_id,
            modified.into(),
            metadata.len(),
        ));
    }
    Ok(sessions)
}

/// The `stat` facts of one session file, `Ok(None)` when there is none.
/// One `stat`, no directory read. The caller has checked the id.
pub(crate) fn metadata(
    store: &ProjectSessions,
    session_id: &str,
) -> Result<Option<SessionMetadata>, SessionError> {
    let metadata = match fs::metadata(session_path(store, session_id)) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(SessionError::Io(err)),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let modified = metadata.modified().map_err(SessionError::Io)?;
    Ok(Some(SessionMetadata::new(
        session_id.to_string(),
        modified.into(),
        metadata.len(),
    )))
}

/// Every entry of a session file in append order. A strict read: an
/// interrupted final line and a duplicated entry id are refused.
pub(crate) fn entries(
    store: &ProjectSessions,
    session_id: &str,
) -> Result<Vec<LogEntry>, SessionError> {
    if !crate::id::is_valid_session_id(session_id) {
        return Err(SessionError::InvalidSessionId(session_id.to_string()));
    }
    Ok(crate::jsonl::read(&session_path(store, session_id), false)?.0)
}

/// The preview of a session file, without its label and archive marker.
/// Reads the whole file.
pub(crate) fn preview(
    store: &ProjectSessions,
    session_id: &str,
    cancel: &dyn Fn() -> bool,
) -> Result<SessionPreview, SessionError> {
    preview_file(session_id, &session_path(store, session_id), cancel)
}

/// Open `path`, walk every JSONL line, and assemble a
/// [`SessionPreview`].
///
/// JSON lines that fail to parse are skipped. Invalid UTF-8 ends the walk at
/// the readable prefix. Either way the entries already read produce a preview,
/// matching the resume-time tolerance for a truncated tail. The walk is
/// one-pass. User text takes precedence over a goal objective regardless of
/// their order in the file.
pub(crate) fn preview_file(
    session_id: &str,
    path: &std::path::Path,
    cancel: &dyn Fn() -> bool,
) -> Result<SessionPreview, SessionError> {
    let metadata = fs::metadata(path)?;
    let modified = metadata
        .modified()
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());

    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut accumulator = PreviewAccumulator::default();
    for (lineno, line) in reader.lines().enumerate() {
        if lineno % crate::SCAN_CANCEL_CHECK_LINES == 0 && cancel() {
            break;
        }
        let Ok(line) = line else { break };
        if let Ok(entry) = serde_json::from_str::<LogEntry>(&line) {
            accumulator.push(&entry);
        }
    }
    Ok(accumulator.finish(session_id, modified))
}

/// The prompts the user submitted in a session file, in file order. An
/// unreadable file yields none.
pub(crate) fn prompts(
    store: &ProjectSessions,
    session_id: &str,
    cancel: &dyn Fn() -> bool,
) -> Vec<RecordedPrompt> {
    scan_file_recorded_prompts(&session_path(store, session_id), cancel)
}

#[cfg(test)]
pub(crate) fn scan_file_user_prompts_cancellable(
    path: &Path,
    cancel: &dyn Fn() -> bool,
) -> Vec<String> {
    scan_file_recorded_prompts(path, cancel)
        .into_iter()
        .map(|p| p.entry.text)
        .collect()
}

/// Read the user-typed prompt texts from one session file, in
/// chronological (file) order.
///
/// A session log is mostly assistant messages and tool results whose
/// bodies dwarf the occasional user prompt. To keep a scan of a large
/// project's logs cheap, each line is first parsed into a tiny
/// [`PromptHead`] capturing only the thread and message role. The
/// expensive full [`LogEntry`] parse (which allocates the
/// message-content tree) runs only for lines that really are top-level
/// user messages.
///
/// Failure-isolation: an unreadable file yields no prompts, and
/// non-UTF-8 or unparseable lines are skipped without aborting the rest
/// of the file.
///
/// `cancel` is polled periodically while reading so a large file doesn't
/// pin a blocking-pool scan after the consumer has gone away.
fn scan_file_recorded_prompts(path: &Path, cancel: &dyn Fn() -> bool) -> Vec<RecordedPrompt> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) => {
            tracing::debug!("skipping unreadable session file {}: {e}", path.display());
            return Vec::new();
        }
    };

    // A missing persisted time uses the session's creation time, then the file's
    // modification time, then the epoch.
    let fallback = path
        .file_stem()
        .and_then(|s| s.to_str())
        .and_then(parse_session_id_created_at)
        .or_else(|| {
            file.metadata()
                .ok()?
                .modified()
                .ok()
                .map(chrono::DateTime::from)
        })
        .unwrap_or_default();
    let mut prompts = Vec::new();
    for (lineno, line) in BufReader::new(file).lines().enumerate() {
        // Cooperative cancellation: this runs on the blocking pool, so we
        // poll `cancel` and stop reading once the consumer is gone. The
        // caller re-checks `cancel` and drops the partial
        // read, so no out-of-order batch is emitted.
        if lineno % crate::SCAN_CANCEL_CHECK_LINES == 0 && cancel() {
            break;
        }
        // A non-UTF-8 (or IO-erroring) line is skipped, not fatal: the
        // failure-isolation property a flat-file format lacks.
        let Ok(line) = line else { continue };
        if line.trim().is_empty() {
            continue;
        }
        let head: PromptHead = match serde_json::from_str(&line) {
            Ok(h) => h,
            Err(e) => {
                tracing::debug!(
                    "skipping unparseable line {} in {}: {e}",
                    lineno + 1,
                    path.display()
                );
                continue;
            }
        };
        if !head.is_user_prompt() {
            continue;
        }
        // Confirmed a top-level user message. The full parse is what
        // actually pulls the text content out. Task notifications
        // are stored with `role:"task_notification"`, so `is_user_prompt`
        // already excluded them above and they never reach here.
        if let Ok(entry) = serde_json::from_str::<LogEntry>(&line)
            && let Some(prompt) = crate::prompt_history::recorded_prompt(&entry, fallback)
        {
            prompts.push(prompt);
        }
    }
    prompts
}

/// A minimal view of one log line: just enough to tell whether it is a
/// top-level user message. Unlisted fields (including the message
/// `content`) are ignored, so serde walks past the heavy body without
/// allocating it.
#[derive(serde::Deserialize)]
struct PromptHead {
    thread: ThreadKind,
    #[serde(default)]
    message: Option<PromptHeadMessage>,
}

#[derive(serde::Deserialize)]
struct PromptHeadMessage {
    #[serde(default)]
    role: Option<String>,
}

impl PromptHead {
    /// A line is a user prompt when it is on the main thread and its
    /// message role is `user`. Assistant / tool-result messages and
    /// non-message entries (system prompt, settings entries) are
    /// excluded.
    fn is_user_prompt(&self) -> bool {
        matches!(self.thread, ThreadKind::Main)
            && self.message.as_ref().and_then(|m| m.role.as_deref()) == Some("user")
    }
}

/// The label a session file's sidecar holds, `Ok(None)` when it has none.
pub(crate) fn label(
    store: &ProjectSessions,
    session_id: &str,
) -> Result<Option<String>, SessionError> {
    directory(store).read_tag(session_id)
}

/// Whether a session file's archive marker exists.
pub(crate) fn archived(store: &ProjectSessions, session_id: &str) -> Result<bool, SessionError> {
    directory(store).read_archived(session_id)
}

/// The label and archive marker a session file's sidecars hold.
pub(crate) fn annotations(
    store: &ProjectSessions,
    session_id: &str,
) -> Result<SessionAnnotations, SessionError> {
    directory(store).annotations(session_id)
}

/// Set or clear the label of a session file, in its sidecar. Refused when
/// the project has no such file. Callers hold the session's lock, which is
/// what orders two writers.
pub(crate) fn set_label(
    store: &ProjectSessions,
    session_id: &str,
    tag: Option<&str>,
) -> Result<(), SessionError> {
    require(store, session_id)?;
    directory(store).write_tag(session_id, tag)
}

/// Archive or unarchive a session file, by its marker. Refused when the
/// project has no such file. Callers hold the session's lock.
pub(crate) fn set_archived(
    store: &ProjectSessions,
    session_id: &str,
    archived: bool,
) -> Result<(), SessionError> {
    require(store, session_id)?;
    directory(store).write_archived(session_id, archived)
}

/// Refuse a session the project has no file of. Only a session that
/// exists has a label or an archive marker to write.
fn require(store: &ProjectSessions, session_id: &str) -> Result<(), SessionError> {
    if !crate::is_valid_session_id(session_id) {
        return Err(SessionError::InvalidSessionId(session_id.to_string()));
    }
    if holds(store, session_id)? {
        return Ok(());
    }
    let missing = crate::sqlite::SqliteError::SessionNotFound(session_id.to_string());
    Err(missing.into())
}

/// The sessions that have a sidecar of one kind, before it is known which
/// of them are session files the database does not hold.
///
/// Two steps, because the caller asks the database about exactly these
/// ids, in one snapshot, between them.
pub(crate) struct Marked<'a> {
    store: &'a ProjectSessions,
    ids: Vec<String>,
    /// Whether a sidecar that cannot be `stat`-ed fails the answer. Only
    /// asked of archive markers.
    strict: bool,
}

/// The sessions with a label sidecar. One directory read and a `stat` per
/// sidecar, never one per session. The labels themselves are not read:
/// [`label`] reads one.
pub(crate) fn labelled(store: &ProjectSessions) -> Result<Marked<'_>, SessionError> {
    Marked::displayed(store, TAG_SIDECAR)
}

/// The sessions with an archive marker, for display: a marker that cannot
/// be `stat`-ed is left out, because a listing that loses one row is still
/// worth rendering.
pub(crate) fn archive_marked(store: &ProjectSessions) -> Result<Marked<'_>, SessionError> {
    Marked::displayed(store, ARCHIVED_SIDECAR)
}

/// The sessions with an archive marker, for a caller whose control flow
/// turns on the answer. A marker that is gone by the time it is `stat`-ed
/// was removed, which is an answer. Any other `stat` failure is not, and
/// is raised: reporting it as unarchived would hand back a session the
/// user put away.
pub(crate) fn archive_marked_strict(store: &ProjectSessions) -> Result<Marked<'_>, SessionError> {
    let ids = store
        .sidecar_candidates(&directory(store).meta_dir(), ARCHIVED_SIDECAR)?
        .into_iter()
        .map(|(session_id, _)| session_id)
        .collect();
    Ok(Marked {
        store,
        ids,
        strict: true,
    })
}

impl<'a> Marked<'a> {
    fn displayed(store: &'a ProjectSessions, extension: &str) -> Result<Self, SessionError> {
        let ids = store
            .session_files(&directory(store).meta_dir(), extension)?
            .into_iter()
            .map(|(session_id, _)| session_id)
            .collect();
        Ok(Self {
            store,
            ids,
            strict: false,
        })
    }

    /// The ids the sidecars name.
    pub(crate) fn ids(&self) -> impl Iterator<Item = &str> {
        self.ids.iter().map(String::as_str)
    }

    /// The ids among them that are session files and that `imported` does
    /// not claim. A sidecar kept beside an imported session, or left behind
    /// without its session file, says nothing.
    pub(crate) fn unimported(
        self,
        imported: impl Fn(&str) -> bool,
    ) -> Result<Vec<String>, SessionError> {
        let meta = directory(self.store).meta_dir();
        let mut ids = Vec::new();
        for session_id in self.ids {
            if imported(&session_id) || !holds(self.store, &session_id)? {
                continue;
            }
            if !self.strict {
                ids.push(session_id);
                continue;
            }
            match fs::metadata(meta.join(format!("{session_id}.{ARCHIVED_SIDECAR}"))) {
                // A directory named like a sidecar is not one, exactly as
                // the display sweep treats it.
                Ok(metadata) => {
                    if metadata.is_file() {
                        ids.push(session_id);
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(ids)
    }
}

/// The import [`crate::SessionLog::resume`] runs for a session the
/// database does not hold, under the caller's writer lock: the file's
/// entries with its label and archive marker, in one transaction. Returns
/// what the read did about an interrupted final entry. A file without
/// entries has nothing to import: the session opens empty and its first
/// commit creates it.
pub(crate) fn import_on_resume(
    store: &ProjectSessions,
    session_id: &str,
) -> Result<Option<TailRepair>, SessionError> {
    let source = directory(store).source(session_id, true)?;
    if !source.entries.is_empty() {
        source.import(store, session_id)?;
    }
    Ok(source.tail_repair)
}

impl ProjectSessions {
    /// Import a session file under the ordinary session lock. Commit switches
    /// authority for entries and annotations together. The file and its
    /// sidecars remain untouched. A session file is never written after its
    /// import, so the database holding the session is the whole answer to
    /// "already imported".
    pub fn migrate_jsonl(&self, session_id: &str) -> Result<MigrationResult, SessionError> {
        let _lock = SessionLock::try_acquire(self, session_id, "migration")?
            .ok_or_else(|| SessionError::MigrationBusy(session_id.into()))?;
        if self.in_database(session_id)? {
            return Ok(MigrationResult::AlreadyImported);
        }
        directory(self)
            .source(session_id, true)?
            .nonempty()?
            .import(self, session_id)?;
        Ok(MigrationResult::Imported)
    }

    /// Validate a possible import without changing the store or creating locks.
    /// This is an advisory snapshot, not a reservation. Actual migration checks
    /// again under the writer lock. SQLite may need its normal WAL read sidecars.
    pub fn check_jsonl_migration(&self, session_id: &str) -> Result<MigrationResult, SessionError> {
        if SessionLock::is_held(self, session_id)? {
            return Err(SessionError::MigrationBusy(session_id.into()));
        }
        if self.in_database(session_id)? {
            return Ok(MigrationResult::AlreadyImported);
        }
        let source = directory(self).source(session_id, true)?.nonempty()?;
        // Run the actual import against a store in memory, not a weaker
        // parallel validator. No persistent database is created for this
        // check.
        source.import_into(&mut SqliteStore::in_memory(self.project_id())?, session_id)?;
        Ok(MigrationResult::WouldImport)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MigrationResult {
    Imported,
    AlreadyImported,
    WouldImport,
}

impl std::fmt::Display for MigrationResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Imported => "imported into SQLite",
            Self::AlreadyImported => "already imported into SQLite",
            Self::WouldImport => "would import into SQLite",
        })
    }
}

#[derive(Debug)]
pub struct MigrationReport {
    pub project_id: String,
    /// Absent when a project directory could not be enumerated.
    pub session_id: Option<String>,
    pub result: Result<MigrationResult, SessionError>,
}

/// Import every session file under `root` and its project directories.
/// Reports each outcome as it completes and continues past individual failures.
/// SQLite files found there are not opened. The root itself covers projects
/// whose bucket name was empty, using the existing directory identity.
pub fn migrate_all_jsonl(
    database: &Path,
    root: &Path,
    dry_run: bool,
    mut report: impl FnMut(MigrationReport),
) -> Result<(), SessionError> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let mut projects = vec![root.to_path_buf()];
    for entry in entries {
        let entry = entry?;
        if entry.path().is_dir() {
            projects.push(entry.path());
        }
    }
    projects.sort();
    for directory in projects {
        let project = directory
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let result = (|| {
            if directory
                .file_name()
                .and_then(|name| name.to_str())
                .is_none()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "project name is not UTF-8",
                )
                .into());
            }
            let store = ProjectSessions::for_project(
                database.to_path_buf(),
                directory.clone(),
                project.clone(),
            );
            let mut sources = fs::read_dir(&directory)?.collect::<Result<Vec<_>, _>>()?;
            sources.sort_by_key(|entry| entry.file_name());
            for source in sources {
                let path = source.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
                    continue;
                }
                let id = path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                let result = (|| {
                    if path.file_stem().and_then(|name| name.to_str()).is_none() {
                        return Err(SessionError::InvalidSessionId(id.clone()));
                    }
                    if !source.metadata()?.is_file() {
                        return Ok(None);
                    }
                    let result = if dry_run {
                        store.check_jsonl_migration(&id)
                    } else {
                        store.migrate_jsonl(&id)
                    }?;
                    Ok(Some(result))
                })();
                match result {
                    Ok(None) => continue,
                    Ok(Some(result)) => report(MigrationReport {
                        project_id: project.clone(),
                        session_id: Some(id),
                        result: Ok(result),
                    }),
                    Err(error) => report(MigrationReport {
                        project_id: project.clone(),
                        session_id: Some(id),
                        result: Err(error),
                    }),
                }
            }
            Ok::<(), SessionError>(())
        })();
        if let Err(error) = result {
            report(MigrationReport {
                project_id: project,
                session_id: None,
                result: Err(error),
            });
        }
    }
    Ok(())
}
