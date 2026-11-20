//! Project-level session discovery.
//!
//! [`ProjectSessions`] is the owner of a project's sessions. It
//! lists them (for `aj list-sessions` and `aj continue`), answers for one
//! session without opening it, and hands [`crate::log::SessionLog`]
//! its connection to the database.
//!
//! The sessions are the ones in the database, plus the session files that
//! were never imported. Every method asks the database first, and
//! [`crate::legacy`] for a session it does not hold. No file of a session
//! is read here.

use aj_models::types::{Message, UserContent};
use chrono::{DateTime, NaiveDateTime, Utc};
use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::legacy;
use crate::log::{LogEntry, LogEntryKind, SessionError, ThreadKind};

pub use crate::legacy::MigrationResult;

/// One project's sessions: where its database is, the project it is scoped
/// to, and the directory that holds its writer locks and any session file
/// that was never imported.
#[derive(Clone)]
pub struct ProjectSessions {
    sessions_dir: PathBuf,
    database_path: PathBuf,
    project_id: String,
}

impl ProjectSessions {
    /// Open an isolated store rooted at `sessions_dir`, with project `default`.
    /// Hosts sharing the global database use [`Self::for_project`] instead.
    pub fn new(sessions_dir: PathBuf) -> Self {
        Self::for_project(
            sessions_dir.join("sessions.sqlite3"),
            sessions_dir,
            "default".into(),
        )
    }

    /// Scope a shared database to one project. `sessions_dir` retains that
    /// project's legacy JSONL files and writer locks. Construction performs no
    /// I/O and never imports an intermediate SQLite database from that directory.
    pub fn for_project(database_path: PathBuf, sessions_dir: PathBuf, project_id: String) -> Self {
        Self {
            sessions_dir,
            database_path,
            project_id,
        }
    }

    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    /// Explicitly discover sibling projects in this database and legacy root.
    /// Missing legacy directories do not hide database-only projects. This
    /// never opens SQLite files found inside individual project directories.
    pub fn projects(&self) -> Result<Vec<Self>, SessionError> {
        let root = self.sessions_dir.parent().unwrap_or(&self.sessions_dir);
        let mut projects = BTreeMap::from([(self.project_id.clone(), self.clone())]);
        let directories = match fs::read_dir(root) {
            Ok(entries) => Some(entries),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        for entry in directories.into_iter().flatten() {
            let entry = entry?;
            if !entry.path().is_dir() || entry.path() == self.sessions_dir {
                continue;
            }
            let name = entry.file_name().into_string().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "project directory name is not UTF-8",
                )
            })?;
            projects.insert(
                name.clone(),
                Self::for_project(self.database_path.clone(), entry.path(), name),
            );
        }
        if let Some(store) = self.sqlite_reader()? {
            for project in store.project_ids()? {
                // Catalog identities become legacy paths here, not at SQL query
                // sites. Refuse names that could escape the legacy store root.
                if Path::new(&project)
                    .file_name()
                    .and_then(|name| name.to_str())
                    != Some(project.as_str())
                {
                    return Err(SessionError::Corrupt(
                        "project identity is not a directory name".into(),
                    ));
                }
                projects.entry(project.clone()).or_insert_with(|| {
                    Self::for_project(self.database_path.clone(), root.join(&project), project)
                });
            }
        }
        Ok(projects.into_values().collect())
    }

    pub fn sqlite_path(&self) -> PathBuf {
        self.database_path.clone()
    }

    pub(crate) fn sqlite_writer(&self) -> Result<crate::sqlite::SqliteStore, SessionError> {
        Ok(crate::sqlite::SqliteStore::open_project(
            &self.database_path,
            &self.project_id,
        )?)
    }

    /// A connection to an existing database for queries, `None` when there
    /// is no database. An unreadable database is an error, never permission
    /// to fall back to a stale JSONL source.
    pub(crate) fn sqlite_reader(&self) -> Result<Option<Reader>, SessionError> {
        Reader::checkout(&self.database_path, &self.project_id)
    }

    /// Every entry of a session in append order, from the database or,
    /// for a session not imported yet, its JSONL file. Takes no writer lock,
    /// so a live session reads one committed snapshot.
    pub fn read_entries(&self, session_id: &str) -> Result<Vec<crate::LogEntry>, SessionError> {
        if self.in_database(session_id)?
            && let Some(store) = self.sqlite_reader()?
        {
            return Ok(store.full_entries(session_id)?);
        }
        legacy::entries(self, session_id)
    }

    /// Make `snapshot` hold `agent`'s page of history before `before`, of
    /// at least `limit` entries, and return where the page starts (see
    /// [`crate::LogSnapshot::page_start`]). Takes no writer lock. A
    /// snapshot that holds its whole session is left as it is.
    pub fn load_page(
        &self,
        snapshot: &mut crate::LogSnapshot,
        agent: aj_agent::events::AgentId,
        before: u64,
        limit: u64,
    ) -> Result<u64, SessionError> {
        if !snapshot.is_complete()
            && let Some(store) = self.sqlite_reader()?
        {
            return Ok(snapshot.load_page(&store, agent, before, limit)?);
        }
        Ok(snapshot.page_start(agent, before, limit))
    }

    /// Make every later label and archive write to this database fail, as a
    /// full disk would, while reads keep working.
    #[cfg(any(test, feature = "test-support"))]
    pub fn refuse_sqlite_annotation_writes(&self) {
        self.sqlite_writer()
            .expect("open the session database")
            .refuse_annotation_writes();
    }

    /// Lift [`Self::refuse_sqlite_annotation_writes`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn allow_sqlite_annotation_writes(&self) {
        self.sqlite_writer()
            .expect("open the session database")
            .allow_annotation_writes();
    }

    /// Make every later save of pending work to this database fail, as a
    /// full disk would, while reads keep working.
    #[cfg(any(test, feature = "test-support"))]
    pub fn refuse_sqlite_pending_writes(&self) {
        self.sqlite_writer()
            .expect("open the session database")
            .refuse_pending_writes();
    }

    /// Lift [`Self::refuse_sqlite_pending_writes`].
    #[cfg(any(test, feature = "test-support"))]
    pub fn allow_sqlite_pending_writes(&self) {
        self.sqlite_writer()
            .expect("open the session database")
            .allow_pending_writes();
    }

    /// Make every later append of an entry to this database fail, or
    /// none, while reads and other writes keep working.
    #[cfg(any(test, feature = "test-support"))]
    pub fn refuse_sqlite_entry_writes(&self, refuse: bool) {
        self.sqlite_writer()
            .expect("open the session database")
            .refuse_every_entry_write(refuse);
    }

    /// Make every later insert or delete of a task record in this database
    /// fail, or neither, while reads keep working.
    #[cfg(any(test, feature = "test-support"))]
    pub fn refuse_sqlite_task_writes(&self, insert: bool, delete: bool) {
        self.sqlite_writer()
            .expect("open the session database")
            .refuse_task_writes(insert, delete);
    }

    /// The id and pid of every task the database records for the session,
    /// whichever thread it belongs to, in id order.
    #[cfg(any(test, feature = "test-support"))]
    pub fn recorded_tasks(&self, session_id: &str) -> Vec<(usize, u32)> {
        self.sqlite_writer()
            .expect("open the session database")
            .all_tasks(session_id)
            .expect("read the task records")
    }

    /// Everything the database stores for the session beside its entries:
    /// its threads with their turns, its pending inputs and its
    /// recorded tasks, one line each.
    #[cfg(any(test, feature = "test-support"))]
    pub fn stored_rows(&self, session_id: &str) -> Vec<String> {
        self.sqlite_writer()
            .expect("open the session database")
            .rows(session_id)
            .expect("read the session's rows")
    }

    /// How many committed transactions the database's write-ahead log
    /// holds. Every write of a session commits one, so the difference
    /// between two readings is how many writes were made in between,
    /// provided no checkpoint restarted the log: one runs when the last
    /// connection closes or the log has grown by a thousand pages.
    #[cfg(any(test, feature = "test-support"))]
    pub fn committed_writes(&self) -> u64 {
        let mut path = self.sqlite_path().into_os_string();
        path.push("-wal");
        let Ok(wal) = std::fs::read(path) else {
            return 0;
        };
        let word = |at: usize| wal.get(at..at + 4).map(|bytes| bytes.try_into().unwrap());
        let Some(page) = word(8).map(u32::from_be_bytes) else {
            return 0;
        };
        let salts = (word(16), word(20));
        let frame = 24 + usize::try_from(page).unwrap();
        let mut commits = 0;
        // A frame of the current log carries the header's salts. A commit
        // frame records the database's size, which is not zero.
        for at in (32..wal.len()).step_by(frame) {
            if (word(at + 8), word(at + 12)) != salts || at + frame > wal.len() {
                break;
            }
            commits += u64::from(word(at + 4) != Some([0; 4]));
        }
        commits
    }

    /// Damage sub-agent `agent`'s stored runs in the session: no entry of
    /// them can be read any more, and their turns are stored as working.
    #[cfg(any(test, feature = "test-support"))]
    pub fn damage_sub_agent(&self, session_id: &str, agent: usize) {
        self.sqlite_writer()
            .expect("open the session database")
            .damage_sub_agent(session_id, agent)
            .expect("damage the sub-agent's runs");
    }

    /// The pid of every command the database records for this project, so a
    /// test can stop what it started. Empty when there is no database.
    #[cfg(any(test, feature = "test-support"))]
    pub fn recorded_task_pids(&self) -> Vec<u32> {
        if !self.database_path.exists() {
            return Vec::new();
        }
        self.sqlite_writer()
            .and_then(|store| Ok(store.all_task_pids()?))
            .unwrap_or_default()
    }

    /// Whether the database holds the session. One that it does not hold
    /// may still exist as a JSONL file that has not been imported.
    pub fn in_database(&self, session_id: &str) -> Result<bool, SessionError> {
        if !crate::is_valid_session_id(session_id) {
            return Err(SessionError::InvalidSessionId(session_id.into()));
        }
        match self.sqlite_reader()? {
            Some(store) => Ok(store.contains_session(session_id)?),
            None => Ok(false),
        }
    }

    /// Storage problems in this store's database, one line each. `None` when
    /// no database exists yet. Read-only, and repairs nothing.
    pub fn check_database(&self) -> Result<Option<Vec<String>>, SessionError> {
        if !self.sqlite_path().exists() {
            return Ok(None);
        }
        Ok(Some(crate::sqlite::check_database(&self.sqlite_path())?))
    }

    /// Every command the database records as running, in every project and
    /// session, each with its project and session id. Empty when no
    /// database exists yet. Read-only: whether a command's process is
    /// still alive is the caller's to find out.
    pub fn task_records(&self) -> Result<Vec<(String, String, crate::TaskRecord)>, SessionError> {
        if !self.sqlite_path().exists() {
            return Ok(Vec::new());
        }
        Ok(crate::sqlite::task_records(&self.sqlite_path())?)
    }

    pub fn sessions_dir(&self) -> &std::path::Path {
        &self.sessions_dir
    }

    /// The session's tag, `Ok(None)` when it has none.
    ///
    /// Callers listing a whole store use [`Self::enumerate_tags`] to load
    /// database labels in bulk.
    pub fn read_tag(&self, session_id: &str) -> Result<Option<String>, SessionError> {
        if !crate::is_valid_session_id(session_id) {
            return Ok(None);
        }
        if self.in_database(session_id)? {
            return Ok(self
                .sqlite_reader()?
                .map(|store| store.annotations(session_id))
                .transpose()?
                .unwrap_or_default()
                .tag);
        }
        legacy::label(self, session_id)
    }

    /// Set or clear the tag of a session the store holds, where it lives.
    /// Callers hold the session's lock, which is what orders two writers.
    ///
    /// A session the store does not hold is refused: one that is not stored
    /// yet keeps its label with its log (see
    /// [`crate::SessionLog::set_tag`]).
    ///
    /// `tag` is expected to have been through [`crate::tag::normalize_tag`]
    /// already, so this writes what it is given.
    ///
    /// An id the grammar rejects is an error rather than a quiet no-op. A
    /// caller that took the `Ok` for a write would go on to publish a label
    /// the store does not hold: the tag command sets the session's row on
    /// the strength of this returning.
    pub fn write_tag(&self, session_id: &str, tag: Option<&str>) -> Result<(), SessionError> {
        if self.in_database(session_id)? {
            self.sqlite_writer()?.set_tag(session_id, tag)?;
            return Ok(());
        }
        legacy::set_label(self, session_id, tag)
    }

    /// Whether the session is archived.
    ///
    /// Callers listing a whole store use [`Self::enumerate_archived`]
    /// instead.
    pub fn read_archived(&self, session_id: &str) -> Result<bool, SessionError> {
        if !crate::is_valid_session_id(session_id) {
            return Ok(false);
        }
        if self.in_database(session_id)? {
            return Ok(self
                .sqlite_reader()?
                .map(|store| store.annotations(session_id))
                .transpose()?
                .unwrap_or_default()
                .archived);
        }
        legacy::archived(self, session_id)
    }

    /// Set or clear the archived bit of a session the store holds, where it
    /// lives. A session the store does not hold is refused, as
    /// [`Self::write_tag`] refuses it. Callers hold the session's lock.
    ///
    /// An id the grammar rejects is an error rather than a quiet no-op, for
    /// the reason [`Self::write_tag`] gives: the archive command sets the
    /// session's row on the strength of this returning.
    pub fn write_archived(&self, session_id: &str, archived: bool) -> Result<(), SessionError> {
        if self.in_database(session_id)? {
            self.sqlite_writer()?.set_archived(session_id, archived)?;
            return Ok(());
        }
        legacy::set_archived(self, session_id, archived)
    }

    /// Tags in this project, without reading transcript entries. Database
    /// labels are already loaded. The label of a session file that was not
    /// imported is left for the caller to read, so live labels and
    /// unreadable individual files retain their own semantics.
    pub fn enumerate_tags(&self) -> Result<Vec<TagListing>, SessionError> {
        let files = legacy::labelled(self)?;
        let sql = self.annotation_catalog(files.ids())?;
        let mut tags: Vec<TagListing> = files
            .unimported(|id| sql.contains_key(id))?
            .into_iter()
            .map(|session_id| TagListing {
                session_id,
                tag: None,
            })
            .collect();
        tags.extend(sql.into_iter().filter_map(|(session_id, annotations)| {
            annotations.tag.map(|tag| TagListing {
                session_id,
                tag: Some(tag),
            })
        }));
        Ok(tags)
    }

    /// Archived IDs for display. An archive marker of a session file that
    /// cannot be read is skipped. Control flow uses the strict archive sweep
    /// instead.
    pub fn enumerate_archived(&self) -> Result<Vec<String>, SessionError> {
        self.archived_with(legacy::archive_marked(self)?)
    }

    /// The archived sessions: the ones the database marks, and the session
    /// files among `files` that it does not hold.
    fn archived_with(&self, files: legacy::Marked<'_>) -> Result<Vec<String>, SessionError> {
        let sql = self.annotation_catalog(files.ids())?;
        let mut archived = files.unimported(|id| sql.contains_key(id))?;
        archived.extend(
            sql.into_iter()
                .filter_map(|(id, annotations)| annotations.archived.then_some(id)),
        );
        Ok(archived)
    }

    /// Every labelled or archived session of the database, plus the sessions
    /// among `file_ids` that it holds, at one snapshot. A session the
    /// database holds takes nothing from a sidecar kept beside it, so a
    /// label or marker cleared in the database stays cleared.
    fn annotation_catalog<'a>(
        &self,
        file_ids: impl Iterator<Item = &'a str>,
    ) -> Result<BTreeMap<String, crate::sqlite::SessionAnnotations>, SessionError> {
        let Some(store) = self.sqlite_reader()? else {
            return Ok(BTreeMap::new());
        };
        Ok(store.annotation_snapshot(file_ids)?.into_iter().collect())
    }

    /// Every per-session file under `dir` carrying `extension`, with the stat
    /// it was found at.
    ///
    /// The sweep shape every per-session axis shares: one directory read plus
    /// one `stat` per file found, never a `stat` per session in the store. A
    /// directory that does not exist reads empty, which is what makes an axis
    /// no session has used cost a single failed `read_dir`.
    ///
    /// Display semantics: a file whose stat fails for any reason is dropped,
    /// because a listing that loses one row is still worth rendering. A caller
    /// whose control flow turns on the answer starts from
    /// [`Self::sidecar_candidates`] and judges each `stat` itself.
    pub(crate) fn session_files(
        &self,
        dir: &Path,
        extension: &str,
    ) -> Result<Vec<(String, fs::Metadata)>, SessionError> {
        let mut found = Vec::new();
        for (session_id, path) in self.sidecar_candidates(dir, extension)? {
            let Ok(metadata) = fs::metadata(&path) else {
                // Vanished since the directory read, or unreadable; either
                // way this axis would rather lose the row than the listing.
                continue;
            };
            // A directory named like one of these files is not one, and
            // offering it would cost a failed open at every enumeration for
            // the life of the store: the read fails, so nothing is cached, so
            // it is tried again next time.
            if !metadata.is_file() {
                continue;
            }
            found.push((session_id, metadata));
        }
        Ok(found)
    }

    /// Every path under `dir` that is named like a per-session `extension`
    /// sidecar, with the session id its stem carries.
    ///
    /// One directory read and no `stat`: this is only the question of what a
    /// name claims to be, which is the half both the display sweep and the
    /// strict sweep agree on. What a caller does about a file that will not
    /// stat is the half they differ on, so it is left to them.
    pub(crate) fn sidecar_candidates(
        &self,
        dir: &Path,
        extension: &str,
    ) -> Result<Vec<(String, PathBuf)>, SessionError> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err.into()),
        };
        let mut found = Vec::new();
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some(extension) {
                continue;
            }
            let Some(session_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !crate::id::is_valid_session_id(session_id) {
                continue;
            }
            found.push((session_id.to_string(), path));
        }
        Ok(found)
    }

    /// The id of every archived session, for a caller whose control flow turns
    /// on the answer.
    ///
    /// The strict counterpart to [`Self::enumerate_archived`]. What it will
    /// not do is report a session unarchived because the store could not be
    /// read (see [`legacy::archive_marked_strict`]).
    fn archived_ids(&self) -> Result<HashSet<String>, SessionError> {
        let archived = self.archived_with(legacy::archive_marked_strict(self)?)?;
        Ok(archived.into_iter().collect())
    }

    /// Every session of the project, latest first, with the facts that
    /// fingerprint it. No entry of a session is read.
    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>, SessionError> {
        let mut sessions = legacy::sessions(self)?;

        if let Some(store) = self.sqlite_reader()? {
            let stored = store.list_sessions()?;
            let imported: HashSet<_> = stored.iter().map(|m| m.id.as_str()).collect();
            sessions.retain(|m| !imported.contains(m.session_id.as_str()));
            sessions.extend(
                stored
                    .into_iter()
                    .map(|m| SessionMetadata::new(m.id, m.updated_at, m.last_seq)),
            );
        }
        // Ids are timestamps, so reverse-lexicographic is latest first.
        sessions.sort_by(|left, right| right.session_id.cmp(&left.session_id));
        Ok(sessions)
    }

    /// The fingerprint of one session, `Ok(None)` when the store holds no
    /// session under that id.
    ///
    /// The single-id form of [`Self::list_sessions`], for the membership
    /// question a lookup asks. It costs one query, and one `stat` when the
    /// database does not hold the session, which is what keeps "is this id
    /// one of mine" off the size of the store.
    ///
    /// An id the grammar rejects answers `Ok(None)` without touching the
    /// filesystem: it cannot name a session in this store, and turning it
    /// into a path is exactly what must not happen (see [`crate::id`]).
    ///
    /// Absence and failure are separate answers. A store this process cannot
    /// read is not a store without the session in it, and a caller that
    /// conflated the two would report a session it holds a directory row for
    /// as gone.
    pub fn session_metadata(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionMetadata>, SessionError> {
        if !crate::id::is_valid_session_id(session_id) {
            return Ok(None);
        }
        if let Some(store) = self.sqlite_reader()?
            && let Some(m) = store.session_metadata(session_id)?
        {
            return Ok(Some(SessionMetadata::new(m.id, m.updated_at, m.last_seq)));
        }
        legacy::metadata(self, session_id)
    }

    /// How large a session's history is, in bytes: the summed length of a
    /// stored session's entries, the length of a session file. `Ok(None)` when
    /// the store holds no session under that id.
    ///
    /// Computed when asked, by reading the session's row headers, so it is
    /// for a caller that shows the size and not for one that polls.
    pub fn session_size(&self, session_id: &str) -> Result<Option<u64>, SessionError> {
        if !crate::id::is_valid_session_id(session_id) {
            return Ok(None);
        }
        if let Some(store) = self.sqlite_reader()?
            && store.contains_session(session_id)?
        {
            return Ok(Some(store.entry_bytes(session_id)?));
        }
        // The revision of a session file is its length.
        Ok(legacy::metadata(self, session_id)?.map(|file| file.revision))
    }

    /// Get the latest session ID, if any exist.
    ///
    /// Archived sessions are passed over. Archiving puts a session away
    /// without closing it, so an explicit id still resumes one; this is the
    /// branch that picks a session nobody named, which is where "put away"
    /// has to mean something. Same rule as bare `aj connect`, which filters
    /// the peer's rows for the same reason. The filter belongs here rather
    /// than in [`Self::list_sessions`], because every listing depends on the
    /// unfiltered read.
    ///
    /// The archive set is read through the strict `archived_ids` sweep rather
    /// than the display one. That is the difference between control flow and
    /// rendering. A listing may lose the archived bit and still be worth
    /// showing. Here the bit decides which session the user gets, so the
    /// same silence would hand back a session the store was told to put
    /// away. Absence of archiving is an answer. Absence of knowledge is
    /// raised.
    pub fn get_latest_session_id(&self) -> Result<Option<String>, SessionError> {
        let sessions = self.list_sessions()?;
        let archived = self.archived_ids()?;
        Ok(sessions
            .iter()
            .find(|metadata| !archived.contains(&metadata.session_id))
            .map(|metadata| metadata.session_id.clone()))
    }

    /// Read one log without loading or locking its session. A sticky `cancel`
    /// drops an interrupted read rather than returning partial counts.
    pub fn session_preview(
        &self,
        session_id: &str,
        cancel: &dyn Fn() -> bool,
    ) -> Result<Option<SessionPreview>, SessionError> {
        if !crate::is_valid_session_id(session_id) {
            return Err(SessionError::InvalidSessionId(session_id.to_string()));
        }
        if cancel() {
            return Ok(None);
        }
        let mut preview = if let Some(store) = self.sqlite_reader()?
            && let Some(metadata) = store.session_metadata(session_id)?
        {
            // Counts and times come from the index. Only the entries that
            // can supply the preview text are read, and only until one does.
            let summary = store.message_summary(session_id)?;
            let mut accumulator = PreviewAccumulator {
                message_count: summary.count,
                last_message_at: summary.last_at,
                ..Default::default()
            };
            store.scan(session_id, "user", true, |entry| {
                accumulator.push_text(&entry);
                accumulator.first_user_message.is_none() && !cancel()
            })?;
            if accumulator.first_user_message.is_none() {
                store.scan(session_id, "goal_change", false, |entry| {
                    accumulator.push_text(&entry);
                    accumulator.first_goal_objective.is_none() && !cancel()
                })?;
            }
            accumulator.finish(session_id, metadata.updated_at)
        } else {
            legacy::preview(self, session_id, cancel)?
        };
        if cancel() {
            return Ok(None);
        }
        // The label and the archived bit are display metadata here: one that
        // cannot be read leaves the preview unlabelled or unarchived rather
        // than failing it.
        preview.tag = self.read_tag(session_id).ok().flatten();
        preview.archived = self.read_archived(session_id).unwrap_or(false);
        Ok(Some(preview))
    }
}

/// A label found without reading the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TagListing {
    pub session_id: String,
    /// The label when the database holds it. `None` for a session file that
    /// was never imported: its label is in a sidecar this listing did not
    /// read, and [`ProjectSessions::read_tag`] reads it.
    pub tag: Option<String>,
}

/// Read connections no reader is using, kept for the next one.
static IDLE_READERS: std::sync::Mutex<Vec<IdleReader>> = std::sync::Mutex::new(Vec::new());

/// How many idle read connections the process keeps. Readers beyond that
/// open a connection and close it.
const MAX_IDLE_READERS: usize = 4;

struct IdleReader {
    store: crate::sqlite::SqliteStore,
    database: PathBuf,
    project: String,
    /// Device and inode of the file the connection was opened on.
    file: (u64, u64),
}

/// A read-only connection to one project's sessions, borrowed from the
/// process's idle connections and returned to them when dropped.
///
/// A connection is reused only for the database file it was opened on. One
/// whose file was replaced or removed is closed, so a reader never answers
/// from a database that is no longer at its path.
pub(crate) struct Reader(Option<IdleReader>);

impl Reader {
    pub(crate) fn checkout(
        database: &std::path::Path,
        project: &str,
    ) -> Result<Option<Self>, SessionError> {
        use std::os::unix::fs::MetadataExt;
        let file = match fs::metadata(database) {
            Ok(metadata) => Some((metadata.dev(), metadata.ino())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let idle = {
            let mut idle = IDLE_READERS.lock().expect("idle readers mutex poisoned");
            // Connections to a file that is no longer at the path are
            // closed here.
            idle.retain(|reader| reader.database != database || Some(reader.file) == file);
            let found = idle
                .iter()
                .position(|reader| reader.database == database && reader.project == project);
            found.map(|index| idle.swap_remove(index))
        };
        let Some(file) = file else {
            return Ok(None);
        };
        if idle.is_some() {
            return Ok(Some(Self(idle)));
        }
        let store = crate::sqlite::SqliteStore::open_project_read_only(database, project)?;
        Ok(Some(Self(Some(IdleReader {
            store,
            database: database.to_owned(),
            project: project.to_owned(),
            file,
        }))))
    }
}

impl std::ops::Deref for Reader {
    type Target = crate::sqlite::SqliteStore;

    fn deref(&self) -> &Self::Target {
        &self.0.as_ref().expect("held until drop").store
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        let Some(reader) = self.0.take() else {
            return;
        };
        let mut idle = IDLE_READERS.lock().expect("idle readers mutex poisoned");
        if idle.len() < MAX_IDLE_READERS {
            idle.push(reader);
        }
    }
}

/// Metadata about a session.
#[derive(Debug, Clone)]
pub struct SessionMetadata {
    pub session_id: String,
    pub modified_at: DateTime<Utc>,
    /// A number that moves when the session's entries do: the append
    /// position of a stored session's last entry, the length of a session
    /// file that was never imported. Paired with `modified_at` this fingerprints the session for
    /// callers caching values derived from it.
    pub revision: u64,
}

impl SessionMetadata {
    pub fn new(session_id: String, modified_at: DateTime<Utc>, revision: u64) -> Self {
        Self {
            session_id,
            modified_at,
            revision,
        }
    }

    /// The modification time, formatted for the `list-sessions` output.
    ///
    /// Derived on demand rather than held: a listing that only fingerprints
    /// files (the session host's directory refresh) enumerates the whole store
    /// on a timer and never renders a row.
    pub fn modified_display(&self) -> String {
        self.modified_at.format("%Y-%m-%d %H:%M:%S UTC").to_string()
    }
}

/// Richer per-session snapshot used by the interactive session
/// selector overlay.
///
/// Unlike [`SessionMetadata`], a [`SessionPreview`] counts the session's
/// `Message` entries and carries the first nonempty user-role text block,
/// falling back to the first nonempty goal objective. For a stored session
/// the counts come from the index and only the entries that can supply the
/// text are read. For a session file that was never imported the whole file
/// is read.
#[derive(Debug, Clone)]
pub struct SessionPreview {
    /// The session id (e.g. `2025-05-11-14-22-03-512`).
    pub session_id: String,
    /// When the session last changed. Held as a real [`DateTime`] (not a
    /// pre-formatted string) so the renderer can choose whatever date/age
    /// formatting it likes.
    pub modified: DateTime<Utc>,
    /// Session creation time. Parsed from `session_id` (which is
    /// minted as a millisecond-precision UTC timestamp on
    /// [`crate::log::SessionLog::create`]). Falls back to
    /// `modified` when the id doesn't parse, so an id this build doesn't
    /// recognise still produces a structurally complete row.
    pub created_at: DateTime<Utc>,
    /// Time of the most recent message-kind entry: the largest
    /// [`LogEntry::timestamp`] on a
    /// [`LogEntryKind::Message`] entry, so out-of-order writes
    /// (e.g. a tool result that completes after a streaming assistant
    /// message finalised) still resolve to the true most-recent message
    /// rather than the last one appended. Falls back to `modified` when no
    /// entry carries a timestamp or no message-kind entry has been
    /// appended yet.
    pub last_message_at: DateTime<Utc>,
    /// Number of [`LogEntryKind::Message`] entries in the
    /// log. User, assistant, and tool_result messages all
    /// contribute; non-message entries (`SystemPrompt`) are
    /// skipped.
    pub message_count: usize,
    /// Display text from the first nonempty main-thread, user-role textual block,
    /// falling back to the first nonempty `GoalChange` objective.
    /// Internal context and delegated task prompts are not user input. Later goal
    /// edits or clears do not replace the fallback. `None` when neither source
    /// has nonempty text.
    /// Text is trimmed but not truncated. This display fallback does not
    /// contribute to `message_count` or `last_message_at`.
    pub first_user_message: Option<String>,
    /// The label the session carries, `None` when it has none.
    ///
    /// It is not an entry of the session, so
    /// [`ProjectSessions::session_preview`] fills it in after the
    /// entries are read.
    pub tag: Option<String>,
    /// Whether the session is archived.
    ///
    /// Filled in like `tag`.
    pub archived: bool,
}

#[derive(Default)]
pub(crate) struct PreviewAccumulator {
    message_count: usize,
    first_user_message: Option<String>,
    first_goal_objective: Option<String>,
    last_message_at: Option<DateTime<Utc>>,
}

impl PreviewAccumulator {
    pub(crate) fn push(&mut self, entry: &LogEntry) {
        if matches!(entry.entry, LogEntryKind::Message { .. }) {
            self.message_count += 1;
            if let Some(timestamp) = entry.timestamp {
                self.last_message_at = Some(
                    self.last_message_at
                        .map_or(timestamp, |previous| previous.max(timestamp)),
                );
            }
        }
        self.push_text(entry);
    }

    /// Take the preview text this entry offers, if none is held yet.
    fn push_text(&mut self, entry: &LogEntry) {
        if let LogEntryKind::Message { message } = &entry.entry {
            if self.first_user_message.is_none()
                && entry.thread == ThreadKind::Main
                && let Some(Message::User(user)) = message.as_stored_wire()
            {
                self.first_user_message = first_user_text(&user.content);
            }
        } else if self.first_goal_objective.is_none()
            && let LogEntryKind::GoalChange { goal: Some(goal) } = &entry.entry
            && !goal.objective.trim().is_empty()
        {
            self.first_goal_objective = Some(goal.objective.trim().to_owned());
        }
    }

    pub(crate) fn finish(self, session_id: &str, modified: DateTime<Utc>) -> SessionPreview {
        SessionPreview {
            session_id: session_id.to_owned(),
            modified,
            created_at: parse_session_id_created_at(session_id).unwrap_or(modified),
            last_message_at: self.last_message_at.unwrap_or(modified),
            message_count: self.message_count,
            first_user_message: self.first_user_message.or(self.first_goal_objective),
            tag: None,
            archived: false,
        }
    }
}

/// Parse a session id minted by [`crate::log::SessionLog::create`]
/// back into the UTC instant it represents.
///
/// The mint format is `%Y-%m-%d-%H-%M-%S-%3f` with an optional
/// `_<N>` collision suffix appended when two `create`s land in the
/// same millisecond. This parser strips the suffix and reads the
/// stem against the same `chrono` format string the minter uses, so
/// the round-trip is exact.
///
/// Returns `None` for any stem that doesn't conform, such as
/// hand-renamed files or future format changes. The caller
/// falls back to file mtime in that case so the row still renders.
pub(crate) fn parse_session_id_created_at(session_id: &str) -> Option<DateTime<Utc>> {
    // Strip a trailing `_<digits>` collision suffix. The mint side
    // never embeds an underscore in the timestamp portion so an
    // underscore unambiguously marks the suffix boundary; we still
    // require the suffix to be all digits to avoid misclassifying
    // an unexpected stem shape as a collision.
    let stem = match session_id.rsplit_once('_') {
        Some((prefix, suffix))
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) =>
        {
            prefix
        }
        _ => session_id,
    };
    NaiveDateTime::parse_from_str(stem, "%Y-%m-%d-%H-%M-%S-%3f")
        .ok()
        .map(|naive| naive.and_utc())
}

/// Return the text from the first [`UserContent::Text`] block in
/// `content`, if any: the user-input text of a preview.
fn first_user_text(content: &[UserContent]) -> Option<String> {
    content.iter().find_map(|b| match b {
        UserContent::Text(t) => {
            let trimmed = t.text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use std::fs::File;

    use crate::jsonl::ARCHIVED_SIDECAR;
    use aj_agent::goal::{Goal, GoalStatus};
    use aj_agent::message::AgentMessage;
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, TextContent, ToolCall, ToolResultMessage,
        UserMessage,
    };
    use tempfile::TempDir;

    use super::*;
    use crate::log::{AgentLog, SessionLog};

    /// Store a session under `id`: one message.
    fn stored_session(persistence: &ProjectSessions, id: &str) {
        let record = LogEntry {
            id: "root".into(),
            parent_id: None,
            timestamp: None,
            thread: ThreadKind::Main,
            agent_id: None,
            entry: LogEntryKind::Message {
                message: AgentMessage::wire(Message::User(UserMessage::text("hello"))),
            },
        };
        let mut store = persistence.sqlite_writer().expect("open the database");
        let (now, none) = (Utc::now(), Default::default());
        crate::jsonl::import_entries(&mut store, id, &[record], now, &none)
            .expect("store the session");
    }

    /// A stored session's tag round-trips, and clearing removes it. A
    /// session the store does not hold has nowhere to keep one.
    #[test]
    fn a_tag_round_trips_and_clears() {
        let (dir, persistence) = fixture();
        let id = "2024-01-01-00-00-00";

        assert!(
            persistence.write_tag(id, Some("fix-auth")).is_err(),
            "a label was accepted for a session that is not stored",
        );
        assert!(persistence.write_archived(id, true).is_err());
        assert!(!persistence.sqlite_path().exists() && !dir.path().join("meta").exists());
        stored_session(&persistence, id);
        assert_eq!(persistence.read_tag(id).expect("read"), None, "untagged");

        persistence.write_tag(id, Some("fix-auth")).expect("write");
        assert_eq!(
            persistence.read_tag(id).expect("read"),
            Some("fix-auth".to_string()),
        );

        persistence.write_tag(id, Some("renamed")).expect("rewrite");
        assert_eq!(
            persistence.read_tag(id).expect("read"),
            Some("renamed".to_string()),
            "a second write replaces the first",
        );

        persistence.write_tag(id, None).expect("clear");
        assert_eq!(persistence.read_tag(id).expect("read"), None);
        persistence
            .write_tag(id, None)
            .expect("clearing an untagged session is not an error");
    }

    /// The sidecar lands at `meta/<session id>.tag`, so
    /// nothing of it appears in the store directory the logs live in and the
    /// session enumeration cannot see it.
    #[test]
    fn a_sidecar_lives_under_meta() {
        let (dir, persistence) = fixture();
        let id = "2024-01-01-00-00-00";
        legacy_file(&persistence, id);
        persistence.write_tag(id, Some("fix-auth")).expect("write");

        assert!(
            dir.path().join("meta").join(format!("{id}.tag")).is_file(),
            "the sidecar is at the path the spec names",
        );
        let mut top: Vec<std::ffi::OsString> = std::fs::read_dir(dir.path())
            .expect("store directory")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .collect();
        top.sort();
        assert_eq!(
            top,
            vec![
                std::ffi::OsString::from(format!("{id}.jsonl")),
                std::ffi::OsString::from("meta"),
            ],
            "nothing landed beside the logs",
        );
        assert_eq!(
            persistence.list_sessions().expect("enumerate").len(),
            1,
            "the tag is not a session of its own",
        );
    }

    /// An id the grammar rejects never becomes a path, on the write side as
    /// much as the read side. The write says so rather than reporting a
    /// success it did not have: a caller that believed it would put a label on
    /// a row the store cannot carry.
    #[test]
    fn an_invalid_id_touches_no_file() {
        let (dir, persistence) = fixture();
        for id in ["../escape", "with/slash", ""] {
            assert!(
                matches!(
                    persistence.write_tag(id, Some("nope")),
                    Err(SessionError::InvalidSessionId(named)) if named == id,
                ),
                "{id:?} is refused rather than silently dropped",
            );
            // Clearing is refused on the same terms: it is a write too.
            assert!(persistence.write_tag(id, None).is_err());
            assert_eq!(persistence.read_tag(id).expect("read"), None);
            assert!(
                matches!(
                    persistence.write_archived(id, true),
                    Err(SessionError::InvalidSessionId(named)) if named == id,
                ),
                "{id:?} is refused by the archived write as well",
            );
            assert!(persistence.write_archived(id, false).is_err());
            assert!(!persistence.read_archived(id).expect("read"));
        }
        assert!(
            !dir.path().join("meta").exists(),
            "no sidecar directory was created for a rejected id",
        );
    }

    /// The write is atomic: the tag arrives whole or not at all. A reader
    /// looking at the directory mid-write sees the previous tag, never a
    /// partial line, because the body is written elsewhere and renamed in.
    #[test]
    fn a_tag_is_never_read_half_written() {
        let (dir, persistence) = fixture();
        let id = "2024-01-01-00-00-00";
        legacy_file(&persistence, id);
        persistence.write_tag(id, Some("before")).expect("write");

        let target = dir.path().join("meta").join(format!("{id}.tag"));
        let before = std::fs::metadata(&target).expect("the sidecar exists");
        persistence
            .write_tag(id, Some("after-a-much-longer-tag"))
            .expect("rewrite");

        // A rename replaces the inode rather than growing the old file, which
        // is what makes a concurrent reader see one whole tag or the other.
        let after = std::fs::metadata(&target).expect("still there");
        assert_ne!(
            inode(&before),
            inode(&after),
            "the rewrite landed as a rename, not an in-place write",
        );
        assert_eq!(
            persistence.read_tag(id).expect("read"),
            Some("after-a-much-longer-tag".to_string()),
        );
        // And nothing is left behind in the directory.
        let strays: Vec<_> = std::fs::read_dir(dir.path().join("meta"))
            .expect("meta")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name())
            .filter(|name| name != &std::ffi::OsString::from(format!("{id}.tag")))
            .collect();
        assert!(
            strays.is_empty(),
            "temporary files were cleaned up: {strays:?}"
        );
    }

    #[cfg(unix)]
    fn inode(metadata: &std::fs::Metadata) -> u64 {
        std::os::unix::fs::MetadataExt::ino(metadata)
    }

    /// Enumerating tags is one directory read, and an untagged store does not
    /// even have the directory.
    #[test]
    fn enumerating_tags_finds_the_sidecars_that_exist() {
        let (_dir, persistence) = fixture();
        assert!(
            persistence.enumerate_tags().expect("enumerate").is_empty(),
            "an untagged store has no meta directory and no tags",
        );

        for (id, tag) in [
            ("2024-01-01-00-00-00", "first"),
            ("2024-01-02-00-00-00", "second"),
        ] {
            legacy_file(&persistence, id);
            persistence.write_tag(id, Some(tag)).expect("write");
        }

        let mut found: Vec<(String, Option<String>)> = persistence
            .enumerate_tags()
            .expect("enumerate")
            .into_iter()
            .map(|tag| (tag.session_id, tag.tag))
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                ("2024-01-01-00-00-00".to_string(), None),
                ("2024-01-02-00-00-00".to_string(), None),
            ],
            "legacy labels are left unread for their caller",
        );
    }

    /// A file in `meta/` that is not a tag sidecar is ignored, so a stray does
    /// not become a row's label. A directory named like one is not a sidecar
    /// either: offering it would cost a failed open at every enumeration for
    /// the life of the store, since a read that fails is deliberately not
    /// cached.
    #[test]
    fn enumerating_tags_ignores_what_is_not_a_sidecar() {
        let (dir, persistence) = fixture();
        legacy_file(&persistence, "2024-01-01-00-00-00");
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).expect("meta");
        std::fs::write(meta.join("notes.txt"), "not a tag").expect("write");
        std::fs::write(meta.join("with slash.tag"), "invalid id").expect("write");
        std::fs::create_dir_all(meta.join("2024-01-01-00-00-00.tag")).expect("a directory");

        assert!(persistence.enumerate_tags().expect("enumerate").is_empty());
        assert!(
            persistence.read_tag("2024-01-01-00-00-00").is_err(),
            "and reading it is the failure the listing spares the caller",
        );
    }

    /// The archived bit round-trips through the sidecar's existence, and
    /// clearing removes the file. Setting it twice is the same answer, and so
    /// is clearing a session that was never archived: the caller states the
    /// bit it wants rather than toggling one.
    #[test]
    fn an_archived_bit_round_trips_and_clears() {
        let (dir, persistence) = fixture();
        let id = "2024-01-01-00-00-00";
        legacy_file(&persistence, id);

        assert!(
            !persistence.read_archived(id).expect("read"),
            "a session with no sidecar is not archived",
        );

        persistence.write_archived(id, true).expect("archive");
        assert!(persistence.read_archived(id).expect("read"));
        assert!(
            dir.path()
                .join("meta")
                .join(format!("{id}.archived"))
                .is_file(),
            "the sidecar lands in meta/, beside the label's and out of the \
             directory the logs live in",
        );

        let sidecar = dir.path().join("meta").join(format!("{id}.archived"));
        let old_mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        File::options()
            .write(true)
            .open(&sidecar)
            .expect("open sidecar")
            .set_times(std::fs::FileTimes::new().set_modified(old_mtime))
            .expect("set old sidecar timestamp");
        let written_at = std::fs::metadata(&sidecar)
            .and_then(|meta| meta.modified())
            .expect("the sidecar's timestamp");
        assert_eq!(written_at, old_mtime);
        persistence
            .write_archived(id, true)
            .expect("archiving an archived session is not an error");
        assert!(persistence.read_archived(id).expect("read"));
        assert_eq!(
            std::fs::metadata(&sidecar)
                .and_then(|meta| meta.modified())
                .expect("the sidecar's timestamp"),
            written_at,
            "and it leaves the file alone rather than rewriting it",
        );

        persistence.write_archived(id, false).expect("unarchive");
        assert!(!persistence.read_archived(id).expect("read"));
        assert!(
            !dir.path()
                .join("meta")
                .join(format!("{id}.archived"))
                .exists(),
            "unarchiving removes the sidecar rather than emptying it",
        );
        persistence
            .write_archived(id, false)
            .expect("unarchiving an unarchived session is not an error");
    }

    /// The two sidecars are two axes over one directory: archiving keeps the
    /// label, and neither listing reports the other's files. A session is
    /// archived by its own sidecar and by nothing else.
    #[test]
    fn the_archived_bit_and_the_label_are_independent() {
        let (_dir, persistence) = fixture();
        let tagged = "2024-01-01-00-00-00";
        let archived = "2024-01-02-00-00-00";
        stored_session(&persistence, tagged);
        stored_session(&persistence, archived);
        persistence
            .write_tag(tagged, Some("fix-auth"))
            .expect("tag");
        persistence.write_archived(tagged, true).expect("archive");
        persistence.write_archived(archived, true).expect("archive");

        assert_eq!(
            persistence.read_tag(tagged).expect("read"),
            Some("fix-auth".to_string()),
            "archiving a session leaves its label alone",
        );
        assert!(
            !persistence
                .read_archived("2024-01-03-00-00-00")
                .expect("read"),
            "and a session nobody archived is not archived",
        );

        let ids = |mut ids: Vec<String>| -> Vec<String> {
            ids.sort();
            ids
        };
        assert_eq!(
            ids(persistence
                .enumerate_tags()
                .expect("enumerate")
                .into_iter()
                .map(|tag| tag.session_id)
                .collect()),
            vec![tagged.to_string()],
            "the label listing reports the tag sidecars and nothing else",
        );
        assert_eq!(
            ids(persistence.enumerate_archived().expect("enumerate")),
            vec![tagged.to_string(), archived.to_string()],
            "and the archived listing reports the archived sidecars",
        );
    }

    /// Enumerating the archived sessions is one directory read that opens no
    /// file, and a store with nothing archived does not even have the
    /// directory. A stray in `meta/` is not the bit, a directory named like a
    /// sidecar least of all: it cannot be removed by an unarchive, so reading
    /// it as archived would strand the session.
    #[test]
    fn enumerating_archived_finds_the_sidecars_that_exist() {
        let (dir, persistence) = fixture();
        assert!(
            persistence
                .enumerate_archived()
                .expect("enumerate")
                .is_empty(),
            "an unarchived store has no meta directory and nothing archived",
        );

        for id in ["2024-01-01-00-00-00", "2024-01-02-00-00-00"] {
            legacy_file(&persistence, id);
        }
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).expect("meta");
        std::fs::write(meta.join("notes.txt"), "not a sidecar").expect("write");
        std::fs::write(meta.join("with slash.archived"), "invalid id").expect("write");
        std::fs::create_dir_all(meta.join("2024-01-01-00-00-00.archived")).expect("a directory");

        assert!(
            persistence
                .enumerate_archived()
                .expect("enumerate")
                .is_empty(),
            "none of those is an archived session",
        );
        assert!(
            !persistence
                .read_archived("2024-01-01-00-00-00")
                .expect("read"),
            "and a directory under the name is not the bit either",
        );
        persistence
            .write_archived("2024-01-01-00-00-00", false)
            .expect("unarchiving what the store already reads as unarchived");

        persistence
            .write_archived("2024-01-02-00-00-00", true)
            .expect("archive");
        let found = persistence.enumerate_archived().expect("enumerate");
        assert_eq!(
            found.iter().map(String::as_str).collect::<Vec<_>>(),
            vec!["2024-01-02-00-00-00"],
        );
        assert_eq!(
            std::fs::metadata(meta.join("2024-01-02-00-00-00.archived"))
                .unwrap()
                .len(),
            0,
            "the legacy archive marker needs no content"
        );
    }

    /// Make `id` a JSONL session that has not been imported, so its label
    /// and archive marker live in sidecars.
    fn legacy_file(persistence: &ProjectSessions, id: &str) {
        std::fs::write(crate::legacy::session_path(persistence, id), "")
            .expect("write the session file");
    }

    /// A JSONL session holding one exchange. Returns its id and path.
    fn legacy_session(persistence: &ProjectSessions, u: &str, a: &str) -> (String, PathBuf) {
        crate::log::test_support::jsonl_session(persistence, |log| {
            append_user_then_assistant(log, u, a)
        })
    }

    /// Build a `ProjectSessions` against a fresh temp dir.
    fn fixture() -> (TempDir, ProjectSessions) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ProjectSessions::new(dir.path().to_path_buf());
        (dir, persistence)
    }

    /// The uncancelled preview of `session_id`, which the test expects to exist.
    fn preview_of(persistence: &ProjectSessions, session_id: &str) -> SessionPreview {
        persistence
            .session_preview(session_id, &|| false)
            .expect("read the preview")
            .expect("an uncancelled read yields a preview")
    }

    fn user_msg(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::User(UserMessage::text(text)))
    }

    fn assistant_text(text: &str) -> AgentMessage {
        AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            })],
            ..AssistantMessage::empty()
        }))
    }

    fn goal(objective: &str) -> Goal {
        Goal {
            id: "goal-1".into(),
            objective: objective.into(),
            status: GoalStatus::Active,
            token_budget: None,
            tokens_used: 0,
            time_used_seconds: 0,
        }
    }

    /// Append one user-text message and one assistant-text message
    /// via the high-level [`AgentLog::add_message`] path.
    fn append_user_then_assistant(log: &mut SessionLog, u: &str, a: &str) {
        let mut view = AgentLog::main(log);
        view.add_message(user_msg(u)).expect("append user");
        view.add_message(assistant_text(a))
            .expect("append assistant");
    }

    #[test]
    fn session_preview_rejects_paths_and_drops_cancelled_reads() {
        let (_dir, persistence) = fixture();
        for id in ["../outside", "a/b", ""] {
            assert!(matches!(
                persistence.session_preview(id, &|| false),
                Err(SessionError::InvalidSessionId(_))
            ));
        }
        let mut log = SessionLog::create(&persistence).expect("log");
        append_user_then_assistant(&mut log, "hello", "reply");
        let polls = std::cell::Cell::new(0);
        let preview = persistence
            .session_preview(log.session_id(), &|| {
                polls.set(polls.get() + 1);
                polls.get() > 1
            })
            .expect("read");
        assert!(polls.get() > 1, "cancelled within the reader");
        assert!(preview.is_none(), "no partial preview escapes");
    }

    #[test]
    fn session_preview_captures_first_user_message_and_count() {
        let (_dir, persistence) = fixture();

        let mut log = SessionLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello world", "hi there");

        // A second message on the main thread so the count crosses 2.
        let mut view = AgentLog::main(&mut log);
        view.add_message(user_msg("follow-up"))
            .expect("append second user");

        let p = preview_of(&persistence, log.session_id());
        assert_eq!(p.session_id, log.session_id());
        assert_eq!(p.message_count, 3);
        assert_eq!(p.first_user_message.as_deref(), Some("hello world"));
    }

    #[test]
    fn previews_fall_back_to_first_nonempty_goal_objective() {
        let (_dir, persistence) = fixture();
        let mut log = SessionLog::create(&persistence).expect("create");
        for objective in [None, Some(""), Some(" \n ")] {
            log.append_goal_change(objective.map(goal)).expect("goal");
        }
        {
            let p = preview_of(&persistence, log.session_id());
            assert_eq!(p.first_user_message, None);
            assert_eq!(p.message_count, 0);
            assert_eq!(p.last_message_at, p.modified);
        }

        log.append_goal_change(Some(goal("  Repair login\nKeep compatibility  ")))
            .expect("submitted goal");
        {
            let p = preview_of(&persistence, log.session_id());
            assert_eq!(
                p.first_user_message.as_deref(),
                Some("Repair login\nKeep compatibility")
            );
            assert_eq!(p.message_count, 0, "an objective is not a message");
            assert_eq!(p.last_message_at, p.modified);
        }

        AgentLog::main(&mut log)
            .add_message(AgentMessage::internal_context("Pursue the goal".into()))
            .expect("internal context");
        let head = log.head().unwrap().clone();
        AgentLog::sub(&mut log, head, 1)
            .add_message(user_msg("Inspect one file for the parent agent"))
            .expect("delegated task prompt");
        let before = persistence
            .session_preview(log.session_id(), &|| false)
            .expect("read")
            .expect("preview");
        log.append_goal_change(Some(goal("Edited objective")))
            .expect("edit goal");
        log.append_goal_change(None).expect("clear goal");
        {
            let p = preview_of(&persistence, log.session_id());
            assert_eq!(
                p.first_user_message.as_deref(),
                Some("Repair login\nKeep compatibility"),
                "internal context, delegated tasks, edits and clears do not replace the objective"
            );
            assert_eq!(
                p.message_count, 2,
                "only internal context and the task are messages"
            );
            assert_eq!(p.last_message_at, before.last_message_at);
        }
    }

    #[test]
    fn previews_prefer_genuine_user_text_over_goal_objectives_in_either_order() {
        for goal_first in [true, false] {
            let (_dir, persistence) = fixture();
            let mut log = SessionLog::create(&persistence).expect("create");
            AgentLog::main(&mut log)
                .add_message(AgentMessage::internal_context("Pursue the goal".into()))
                .expect("internal context");
            if goal_first {
                log.append_goal_change(Some(goal("Repair login")))
                    .expect("goal");
            }
            append_user_then_assistant(&mut log, "  Focus on the timeout  ", "Understood");
            if !goal_first {
                log.append_goal_change(Some(goal("Repair login")))
                    .expect("goal");
            }
            AgentLog::main(&mut log)
                .add_message(user_msg("Second user prompt"))
                .expect("follow-up");
            {
                let p = preview_of(&persistence, log.session_id());
                assert_eq!(
                    p.first_user_message.as_deref(),
                    Some("Focus on the timeout")
                );
                assert_eq!(p.message_count, 4);
            }
        }
    }

    /// A file whose stem is not a session id is not a session: the store can
    /// never be asked about it again (the grammar rejects the id at every
    /// lookup), so a row for it would be a directory entry nothing resolves.
    #[test]
    fn enumeration_skips_a_stem_that_is_not_a_session_id() {
        let (_dir, persistence) = fixture();
        let mut log = SessionLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let real = log.session_id().to_string();
        drop(log);
        for stem in ["not a session", "sneaky.name", "héllo"] {
            std::fs::write(
                persistence.sessions_dir().join(format!("{stem}.jsonl")),
                "{}\n",
            )
            .expect("write a stray log");
        }

        let listed: Vec<String> = persistence
            .list_sessions()
            .expect("enumerate")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(listed, vec![real]);
    }

    /// The single-id membership primitive: one `stat`, and nothing at all for
    /// an id the grammar rejects.
    ///
    /// The traversal case puts a real, readable log where the id points, so
    /// the refusal has to come from the grammar rather than from the file not
    /// being there.
    #[test]
    fn session_metadata_answers_one_id_and_refuses_a_traversal() {
        // A store one level down, so the traversal target below still lives
        // inside the temp dir.
        let dir = TempDir::new().expect("temp dir");
        let persistence = ProjectSessions::new(dir.path().join("sessions"));
        let (session_id, path) = legacy_session(&persistence, "hello", "hi");
        let size = std::fs::metadata(path).expect("stat").len();

        let metadata = persistence
            .session_metadata(&session_id)
            .expect("the stat succeeded")
            .expect("the log is there");
        assert_eq!(metadata.session_id, session_id);
        assert_eq!(metadata.revision, size);
        assert_eq!(
            persistence.session_size(&session_id).expect("the size"),
            Some(size)
        );

        assert!(
            persistence
                .session_metadata("2026-01-01-00-00-00-000")
                .expect("a missing log is not a failure")
                .is_none(),
        );

        // A readable log one directory up from the store. Only the grammar
        // stands between the id that names it and a `stat` that resolves.
        std::fs::create_dir_all(dir.path().join("elsewhere")).expect("sibling dir");
        std::fs::write(dir.path().join("elsewhere/reachable.jsonl"), "{}\n")
            .expect("a log outside the store");
        assert!(
            dir.path().join("elsewhere/reachable.jsonl").is_file(),
            "the traversal target is really there",
        );
        for id in ["../elsewhere/reachable", "..", ""] {
            assert!(
                persistence
                    .session_metadata(id)
                    .expect("a rejected id is not a failure")
                    .is_none(),
                "{id:?} resolved to a file",
            );
        }
    }

    /// Restores a path's permissions when it goes out of scope.
    ///
    /// A test that drops permission bits and restores them at the end leaves
    /// the directory unsearchable if an assertion between the two panics, and
    /// then the `TempDir` cannot be removed either. Ownership is the fix: the
    /// restore rides on unwinding.
    #[cfg(unix)]
    struct PermissionGuard {
        path: PathBuf,
        restore: fs::Permissions,
    }

    #[cfg(unix)]
    impl PermissionGuard {
        /// Drop every permission bit on `path`, restoring on drop.
        fn seal(path: &Path) -> Self {
            use std::os::unix::fs::PermissionsExt;

            let restore = fs::metadata(path)
                .expect("stat before sealing")
                .permissions();
            fs::set_permissions(path, fs::Permissions::from_mode(0o000))
                .expect("drop the permission bits");
            Self {
                path: path.to_path_buf(),
                restore,
            }
        }
    }

    #[cfg(unix)]
    impl Drop for PermissionGuard {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, self.restore.clone());
        }
    }

    /// The default pick passes over an archived session, and archiving is not
    /// closing: the row still lists, the bit is reversible, and only the pick
    /// filters. Bare `aj connect` already reads its peer's rows this way, and
    /// this is the local analogue.
    #[test]
    fn the_default_pick_passes_over_an_archived_session() {
        let (_dir, persistence) = fixture();
        let (older, _) = legacy_session(&persistence, "older", "hi");

        // The younger log is written by hand rather than minted: ids come from
        // the clock, so two creates inside one test can collide, and the
        // enumeration orders by id. A file has to exist for the id to list,
        // and a fresh log writes nothing until its first append.
        let newer = "2999-01-01-00-00-00-000";
        std::fs::copy(
            persistence.sessions_dir().join(format!("{older}.jsonl")),
            persistence.sessions_dir().join(format!("{newer}.jsonl")),
        )
        .expect("write a younger log");

        assert_eq!(
            persistence.get_latest_session_id().expect("pick"),
            Some(newer.to_string()),
            "the younger session is the default pick",
        );

        persistence.write_archived(newer, true).expect("archive");
        assert_eq!(
            persistence.get_latest_session_id().expect("pick"),
            Some(older.clone()),
            "archiving the younger one hands the pick to the older",
        );
        assert_eq!(
            persistence.list_sessions().expect("list").len(),
            2,
            "and the listing still lists both, because only the pick filters",
        );

        persistence.write_archived(newer, false).expect("unarchive");
        assert_eq!(
            persistence.get_latest_session_id().expect("pick"),
            Some(newer.to_string()),
            "clearing the bit hands the pick back",
        );

        persistence.write_archived(newer, true).expect("archive");
        persistence.write_archived(&older, true).expect("archive");
        assert_eq!(
            persistence.get_latest_session_id().expect("pick"),
            None,
            "with every session put away the pick is empty, as against an empty store",
        );
    }

    /// An archive set that cannot be read is not an empty archive set.
    ///
    /// The unnamed pick is control flow, so a lost archived bit does not
    /// degrade the answer, it inverts it: the session the store was told to
    /// put away is exactly the one handed back. A listing may show the bit
    /// wrong and still be worth showing, which is why the display helper
    /// swallows the failure and this branch may not.
    #[cfg(unix)]
    #[test]
    fn the_default_pick_raises_when_the_archive_set_cannot_be_read() {
        let (_dir, persistence) = fixture();
        let (older, _) = legacy_session(&persistence, "older", "hi");
        let newer = "2999-01-01-00-00-00-000";
        std::fs::copy(
            persistence.sessions_dir().join(format!("{older}.jsonl")),
            persistence.sessions_dir().join(format!("{newer}.jsonl")),
        )
        .expect("write a younger log");
        persistence.write_archived(newer, true).expect("archive");

        let meta = crate::jsonl::Directory(persistence.sessions_dir()).meta_dir();
        let pick = {
            let _sealed = PermissionGuard::seal(&meta);
            if fs::read_dir(&meta).is_ok() {
                // Root ignores the read bit, so the failure this test is about
                // cannot be produced. Skipping beats asserting something that
                // cannot fail.
                return;
            }
            persistence.get_latest_session_id()
        };

        assert!(
            pick.is_err(),
            "an unreadable archive set must raise rather than hand back {newer}: {pick:?}",
        );

        assert_eq!(
            persistence.get_latest_session_id().expect("pick"),
            Some(older),
            "and the pick is answerable again once the set can be read",
        );
    }

    /// A sidecar the store cannot stat is not a sidecar that is gone.
    ///
    /// The directory read succeeding is not the whole answer: the archived bit
    /// is one `stat` per sidecar, and a permission denial or an I/O error
    /// there loses exactly the bit that decides which session the user gets.
    /// Only `NotFound` means the sidecar vanished between the read and the
    /// stat, which is a session unarchived as of this read.
    #[cfg(unix)]
    #[test]
    fn the_default_pick_raises_when_one_sidecar_cannot_be_stat_ed() {
        let (_dir, persistence) = fixture();
        let (older, _) = legacy_session(&persistence, "older", "hi");
        let newer = "2999-01-01-00-00-00-000";
        std::fs::copy(
            persistence.sessions_dir().join(format!("{older}.jsonl")),
            persistence.sessions_dir().join(format!("{newer}.jsonl")),
        )
        .expect("write a younger log");
        persistence.write_archived(newer, true).expect("archive");

        // The sidecar is replaced by a symlink into a directory that cannot be
        // searched, so `meta/` still reads and only this entry's stat fails.
        // A dangling symlink would be NotFound, which is the case that
        // legitimately means "gone".
        let sidecar = crate::jsonl::Directory(persistence.sessions_dir())
            .meta_dir()
            .join(format!("{newer}.{ARCHIVED_SIDECAR}"));
        let vault = persistence.sessions_dir().join("vault");
        std::fs::create_dir(&vault).expect("vault");
        let hidden = vault.join("target");
        std::fs::write(&hidden, b"").expect("target");
        std::fs::remove_file(&sidecar).expect("drop the real sidecar");
        std::os::unix::fs::symlink(&hidden, &sidecar).expect("symlink the sidecar");
        let _sealed = PermissionGuard::seal(&vault);
        if fs::metadata(&sidecar).is_ok() {
            // Root ignores the search bit, so the failure this test is about
            // cannot be produced. Skipping beats asserting something that
            // cannot fail.
            return;
        }

        let pick = persistence.get_latest_session_id();
        assert!(
            pick.is_err(),
            "a sidecar that cannot be stat-ed must raise rather than read as unarchived: {pick:?}",
        );
    }

    /// Listing is a names-and-stats operation. A validly named `.jsonl` is a
    /// session regardless of its contents, while unrelated files and
    /// directories remain outside the store.
    #[test]
    fn listing_does_not_parse_session_contents() {
        let (_dir, persistence) = fixture();
        let mut log = SessionLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hello", "hi");
        let session_id = log.session_id().to_string();
        drop(log);
        let malformed = persistence
            .sessions_dir()
            .join("2000-01-01-00-00-00-000.jsonl");
        std::fs::write(&malformed, "not json at all\n").expect("write a malformed log");
        // Neither an unrelated extension nor a directory is a session.
        std::fs::write(persistence.sessions_dir().join("host-id"), "id\n").expect("write");
        std::fs::create_dir(persistence.sessions_dir().join("nested.jsonl")).expect("mkdir");

        let listed: Vec<String> = persistence
            .list_sessions()
            .expect("list")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(
            listed,
            vec![session_id.clone(), "2000-01-01-00-00-00-000".to_string()],
            "both session files are listed, latest first",
        );
    }

    /// Listing never opens a log, which is what the host's directory build
    /// depends on: an unreadable log is still a session.
    #[cfg(unix)]
    #[test]
    fn listing_opens_no_log() {
        use std::os::unix::fs::PermissionsExt;

        let (_dir, persistence) = fixture();
        let (session_id, path) = legacy_session(&persistence, "hello", "hi");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))
            .expect("drop the read bit");
        if File::open(&path).is_ok() {
            // Root ignores the permission bits, so there is nothing to prove
            // here. Skipping beats asserting something that cannot fail.
            return;
        }

        let listed: Vec<String> = persistence
            .list_sessions()
            .expect("list")
            .into_iter()
            .map(|metadata| metadata.session_id)
            .collect();
        assert_eq!(
            listed,
            vec![session_id.clone()],
            "a log nothing can open is still listed, so nothing opened it",
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("restore the read bit");
    }

    /// A preview carries the label its session's sidecar holds, and only that
    /// session's.
    #[test]
    fn previews_carry_the_labels_the_sidecars_hold() {
        let (_dir, persistence) = fixture();
        let mut ids = Vec::new();
        for i in 0..3 {
            let mut log = SessionLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            ids.push(log.session_id().to_string());
        }
        persistence
            .write_tag(&ids[1], Some("fix-auth"))
            .expect("write");

        let labels: Vec<Option<String>> = ids
            .iter()
            .map(|id| preview_of(&persistence, id).tag)
            .collect();
        assert_eq!(labels, vec![None, Some("fix-auth".to_string()), None]);
    }

    /// A preview carries the archived bit off its session's sidecar. It is the
    /// only place a preview can learn the bit: the per-file walk reads the
    /// log, which never held it.
    #[test]
    fn previews_carry_the_archived_bit() {
        let (_dir, persistence) = fixture();
        let mut ids = Vec::new();
        for i in 0..2 {
            let mut log = SessionLog::create(&persistence).expect("create");
            append_user_then_assistant(&mut log, &format!("prompt {i}"), &format!("reply {i}"));
            ids.push(log.session_id().to_string());
        }
        persistence.write_archived(&ids[1], true).expect("archive");

        let archived: Vec<bool> = ids
            .iter()
            .map(|id| preview_of(&persistence, id).archived)
            .collect();
        assert_eq!(archived, vec![false, true]);
    }

    /// A sidecar body that reads as no label at all leaves the session
    /// unlabelled rather than failing the preview, matching how the sidecar
    /// read tolerates a hand-edited file.
    #[test]
    fn an_unusable_sidecar_leaves_the_session_unlabelled() {
        let (dir, persistence) = fixture();
        let mut log = SessionLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "prompt", "reply");
        let id = log.session_id().to_string();
        let meta = dir.path().join("meta");
        std::fs::create_dir_all(&meta).expect("meta");
        std::fs::write(meta.join(format!("{id}.tag")), "\n").expect("write");

        assert_eq!(preview_of(&persistence, &id).tag, None);
    }

    #[test]
    fn read_session_preview_file_stops_mid_file() {
        // A single file larger than the in-file poll interval, so the check
        // inside the read loop is what bails.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("2024-01-01-00-00-00.jsonl");
        let n = crate::SCAN_CANCEL_CHECK_LINES * 3;
        {
            use std::io::Write;
            let mut file = std::fs::File::create(&path).unwrap();
            for i in 0..n {
                let line = serde_json::to_string(&serde_json::json!({
                    "id": format!("{i:08}"),
                    "thread": "user",
                    "type": "message",
                    "message": {
                        "role": "user",
                        "content": [{"type": "text", "text": format!("p{i}")}],
                        "timestamp": 0,
                    },
                }))
                .unwrap();
                writeln!(file, "{line}").unwrap();
            }
        }

        let full =
            crate::legacy::preview_file("2024-01-01-00-00-00", &path, &|| false).expect("read");
        assert_eq!(full.message_count, n);

        // Sticky predicate: false at the line-0 poll, true from line-1024 on.
        let calls = std::cell::Cell::new(0usize);
        let cancel = || {
            let c = calls.get();
            calls.set(c + 1);
            c > 0
        };
        let partial =
            crate::legacy::preview_file("2024-01-01-00-00-00", &path, &cancel).expect("read");
        assert_eq!(
            partial.message_count,
            crate::SCAN_CANCEL_CHECK_LINES,
            "in-file cancel bails at the 1024-line poll, not after the full read"
        );
    }

    #[test]
    fn session_preview_ignores_non_user_first_messages() {
        let (_dir, persistence) = fixture();
        let mut log = SessionLog::create(&persistence).expect("create");
        // First message is a tool_result (not a user prompt). The
        // preview should leave `first_user_message` at `None`.
        let mut view = AgentLog::main(&mut log);
        view.add_message(AgentMessage::wire(Message::ToolResult(
            ToolResultMessage::text("x", "ping", "ok", false),
        )))
        .expect("append");

        let p = preview_of(&persistence, log.session_id());
        assert!(p.first_user_message.is_none());
        assert_eq!(p.message_count, 1);
    }

    /// A log that is not valid UTF-8 still yields a preview, with nothing read
    /// from it, rather than failing.
    #[test]
    fn session_preview_keeps_a_malformed_session() {
        let (_dir, persistence) = fixture();
        let sessions_dir = persistence.sessions_dir().to_path_buf();
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        std::fs::write(sessions_dir.join("old.jsonl"), [0xff, 0xfe, 0xff])
            .expect("write malformed session");

        let malformed = preview_of(&persistence, "old");
        assert_eq!(malformed.message_count, 0);
        assert!(malformed.first_user_message.is_none());
    }

    #[test]
    fn read_session_preview_file_tolerates_a_malformed_line() {
        let (_dir, persistence) = fixture();
        let (session_id, path) = legacy_session(&persistence, "hello", "hi");

        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .expect("read log")
            .lines()
            .map(str::to_string)
            .collect();
        // A malformed first line has no special meaning: later entries still
        // contribute to the preview.
        lines.insert(0, "}{ this is not json".to_string());
        std::fs::write(&path, format!("{}\n", lines.join("\n"))).expect("rewrite");

        let preview = crate::legacy::preview_file(&session_id, &path, &|| false).expect("read");
        // The two messages survive. Only the torn line is skipped.
        assert_eq!(preview.message_count, 2);
        assert_eq!(preview.first_user_message.as_deref(), Some("hello"));
    }

    #[test]
    fn parse_session_id_created_at_round_trips_minted_id() {
        let parsed = super::parse_session_id_created_at("2025-05-11-14-22-03-512")
            .expect("known-good stem parses");
        let expected = chrono::NaiveDate::from_ymd_opt(2025, 5, 11)
            .unwrap()
            .and_hms_milli_opt(14, 22, 3, 512)
            .unwrap()
            .and_utc();
        assert_eq!(parsed, expected);
    }

    #[test]
    fn parse_session_id_created_at_strips_collision_suffix() {
        let suffix = super::parse_session_id_created_at("2025-05-11-14-22-03-512_3")
            .expect("suffixed stem parses");
        let bare = super::parse_session_id_created_at("2025-05-11-14-22-03-512")
            .expect("bare stem parses");
        assert_eq!(suffix, bare);
    }

    #[test]
    fn parse_session_id_created_at_returns_none_for_unrecognised_stem() {
        assert!(super::parse_session_id_created_at("custom-name").is_none());
        assert!(super::parse_session_id_created_at("").is_none());
        assert!(super::parse_session_id_created_at("2025-05-11-14-22-03-512_abc").is_none());
    }

    #[test]
    fn session_preview_populates_created_at_from_session_id() {
        let (_dir, persistence) = fixture();
        let mut log = SessionLog::create(&persistence).expect("create");
        append_user_then_assistant(&mut log, "hi", "ok");
        let session_id = log.session_id().to_string();

        let p = preview_of(&persistence, &session_id);
        let expected =
            super::parse_session_id_created_at(&session_id).expect("freshly-minted id parses");
        assert_eq!(p.created_at, expected);
    }

    #[test]
    fn session_preview_counts_tool_result_entries() {
        let (_dir, persistence) = fixture();
        let mut log = SessionLog::create(&persistence).expect("create");
        {
            let mut view = AgentLog::main(&mut log);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    is_raw: false,
                    id: "tu-1".into(),
                    name: "ping".into(),
                    arguments: serde_json::json!({}),
                })],
                ..AssistantMessage::empty()
            })))
            .expect("a");
            view.add_message(AgentMessage::wire(Message::ToolResult(
                ToolResultMessage::text("tu-1", "ping", "ok", false),
            )))
            .expect("tr");
        }

        // Three wire-level messages: user, assistant, tool_result.
        assert_eq!(preview_of(&persistence, log.session_id()).message_count, 3);
    }

    #[test]
    fn session_preview_falls_back_to_modified_when_no_message_entries() {
        // Legacy on-disk shape: a session file containing only a
        // SystemPrompt entry, with no `Message` entries. New code
        // can't produce this layout (the system prompt buffers and
        // never flushes alone), but files written by older builds
        // still exist on users' disks and the preview walk must
        // render them gracefully. The fallback under test:
        // `last_message_at` defaults to the file mtime when no
        // Message-kind entry contributed a timestamp.
        let (_dir, persistence) = fixture();
        let sessions_dir = persistence.sessions_dir().to_path_buf();
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");

        let session_id = "2024-01-01-00-00-00-000";
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        let line = serde_json::json!({
            "id": "00000000",
            "timestamp": "2024-01-01T00:00:00Z",
            "thread": "meta",
            "type": "system_prompt",
            "text": "legacy abandoned-session prompt",
        });
        std::fs::write(&path, format!("{line}\n")).expect("write legacy file");

        let p = preview_of(&persistence, session_id);
        assert_eq!(p.message_count, 0);
        assert_eq!(p.last_message_at, p.modified);
    }

    #[test]
    fn session_preview_uses_largest_message_timestamp() {
        let (_dir, persistence) = fixture();
        let mut entries = Vec::new();
        let (session_id, path) = crate::log::test_support::jsonl_session(&persistence, |log| {
            append_user_then_assistant(log, "hello", "world");
            let mut view = AgentLog::main(log);
            view.add_message(user_msg("follow-up"))
                .expect("append user2");
            entries = log.entries_in_order().into_iter().cloned().collect();
        });

        // Persist out-of-order message times so neither file order nor mtime
        // can stand in for the largest message timestamp.
        let base = "2024-01-01T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        let mut offsets = [1, 3, 2].into_iter();
        let mut lines = String::new();
        for entry in &mut entries {
            let offset = if matches!(entry.entry, crate::log::LogEntryKind::Message { .. }) {
                offsets.next().expect("three message entries")
            } else {
                4
            };
            entry.timestamp = Some(base + chrono::Duration::seconds(offset));
            lines.push_str(&serde_json::to_string(entry).expect("serialize entry"));
            lines.push('\n');
        }
        assert!(
            offsets.next().is_none(),
            "all three message times were used"
        );
        std::fs::write(path, lines).expect("persist fixture timestamps");

        let p = preview_of(&persistence, &session_id);
        assert_eq!(p.message_count, 3);
        assert_eq!(p.last_message_at, base + chrono::Duration::seconds(3));
    }
}
