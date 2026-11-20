//! Project-scoped, append-only session entries in a shared database.
//!
//! A session's entries are filed under threads: the main thread,
//! each branch of it and each sub-agent run is one line of rows with
//! its own id and a pointer to the thread and entry it continues
//! from. The first thread also holds the session's meta entries, the
//! system prompt first. Every row keeps its whole entry and its
//! session-wide position, so reading a session in append order
//! returns exactly what was written.
//!
//! A branch is read by thread: [`SqliteStore::entries`] reads a
//! stretch of one, and [`SqliteStore::walk`] visits stretches of several
//! in append order without reading them. Neither passes over a row of a
//! thread it was not asked for. Reading a whole session in append
//! order is for the readers that want all of it.
//!
//! The store decides where an entry belongs. A writer hands over the entry,
//! which names the agent it is for and the entry it continues from, and the
//! append's transaction files it (see [`place`]), assigns its position and
//! answers where it landed. An import files a whole session by the same rule.
//!
//! One host writes a session at a time, which the session lock enforces. The
//! store does not acquire that lock, repair history, or decide when a session
//! becomes durable. Callers own those policies.
//!
//! A write that finds the database locked by another connection for longer
//! than the busy timeout fails with an error [`SqliteError::is_busy`]
//! recognizes. It has changed nothing and can be tried again.

use std::path::Path;
use std::sync::LazyLock;
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use thiserror::Error;

use crate::log::{LogEntry, LogEntryKind, ThreadKind};
use crate::threads::{self, Class, Owner};

/// How long a statement waits for another connection's lock unless its
/// store is told otherwise (see [`SqliteStore::set_busy_timeout`]).
pub(crate) const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// Largest write-ahead log left on disk after a checkpoint, in bytes.
const WAL_SIZE_LIMIT: i64 = 64 * 1024 * 1024;

/// `('a', 'b')`: row types as an SQL list.
fn sql_list(types: &[&[&str]]) -> String {
    let quoted: Vec<String> = types
        .iter()
        .flat_map(|types| types.iter())
        .map(|kind| format!("'{kind}'"))
        .collect();
    format!("({})", quoted.join(", "))
}

/// Row types of messages the agents exchange.
static MESSAGES: LazyLock<String> =
    LazyLock::new(|| sql_list(&[threads::MESSAGE_TYPES, threads::TOOL_OUTPUT_TYPES]));

/// Selects the state entries: `type IN (...)` over
/// [`threads::STATE_TYPES`]. The list decides which rows a query returns.
/// The `entries_state` index only makes that fast, and only while its
/// frozen list is this one: a query with another list reads the same
/// rows through `entries_path`.
static STATE: LazyLock<String> =
    LazyLock::new(|| format!("type IN {}", sql_list(&[threads::STATE_TYPES])));

/// The next row of a span for [`SqliteStore::walk`]: position and type,
/// read from `entries_path` alone, in the index's own order.
fn walk_sql(descending: bool) -> String {
    let order = if descending { "DESC" } else { "ASC" };
    format!(
        "SELECT seq, type FROM entries INDEXED BY entries_path
         WHERE thread = ?1 AND seq > ?2 AND seq <= ?3
           AND (?4 IS NULL OR type = ?4)
         ORDER BY seq {order} LIMIT 1"
    )
}

/// The entries of a span for [`SqliteStore::entries`], all of them or the
/// state entries. The state query names no index: the planner takes
/// `entries_state` while its list is this build's, and `entries_path`
/// otherwise.
fn entries_sql(state: bool) -> String {
    if state {
        format!(
            "SELECT seq, entry FROM entries
             WHERE thread = ?1 AND seq > ?2 AND seq <= ?3 AND {}
             ORDER BY seq",
            *STATE
        )
    } else {
        "SELECT seq, entry FROM entries INDEXED BY entries_path
         WHERE thread = ?1 AND seq > ?2 AND seq <= ?3 ORDER BY seq"
            .to_owned()
    }
}

/// What each model response of a span reported, for
/// [`SqliteStore::usage`]. The two expressions are the keys of
/// `entries_usage`, written as the index writes them, so the index
/// answers the query and no entry is read.
const USAGE_SQL: &str = "SELECT seq, json_extract(entry, '$.message.usage'),
            json_extract(entry, '$.message.stop_reason')
     FROM entries INDEXED BY entries_usage
     WHERE thread = ?1 AND seq > ?2 AND seq <= ?3 AND type = 'assistant'
     ORDER BY seq";

/// Applied in order, each at most once, and counted in `PRAGMA
/// user_version`. A released migration is never edited or reordered, not
/// even in its whitespace: a schema change is a new one at the end. Each is
/// a literal, so nothing a later build changes elsewhere can alter it, and
/// a test pins the fingerprint of every released prefix.
const MIGRATIONS: &[(&str, &str)] = &[("001-initial", INITIAL_001)];

/// `sessions.name` is the session id users see. `sessions.last_seq` is the
/// position of the session's last entry. Which main thread a reopen lands
/// on is read off the threads (see [`SqliteStore::main_with_pending`]).
///
/// A thread's id is assigned by SQLite, so within a session id order is the
/// order threads started. `parent` and `forked_at` name the fork point, the
/// entry a thread continues from (a branch) or was spawned at (a sub-agent
/// run): its thread and its `seq`. `last_seq` is the `seq` of the thread's
/// last entry, not counting the meta entries filed beside it (see
/// [`Landing::Beside`]). A thread may hold no entry: it is created with the
/// first input accepted for it (see [`SqliteStore::create_thread`]), and
/// the entry that input becomes follows. Its `last_seq` is then
/// `forked_at`, and `0` for the session's first thread. `turn` is the
/// stored state of the thread's turn.
///
/// An entry is found five ways. Every append maintains the first three
/// indexes, the append of a state entry `entries_state` too, and the
/// append of a response `entries_usage`:
///
/// - the primary key `(session, seq)` reads a whole session in append
///   order and one entry by position
/// - the unique `(session, entry_id)` finds an entry by its id
/// - `entries_path` reads one thread over a range of positions, which is
///   how a branch is read: its thread, then the one it continues from up
///   to the fork point, and so on. It carries the row type, so choosing
///   entries reads none of them.
/// - `entries_state` holds the state entries of each thread alone, so the
///   ones before a reader's window are found without passing over the
///   entries between them. Its list of types is frozen here. A type a
///   later build adds is not in it, and a query that asks for that type
///   too is answered from `entries_path` (see [`STATE`]).
/// - `entries_usage` holds, for each model response, the usage it
///   reported and why it stopped, read out of the entry. A reader sums
///   the usage before its window from it. The usage is stored nowhere
///   else outside the entry. A build that reads other fields is answered
///   from the entries (see [`USAGE_SQL`]).
///
/// Deleting a session row deletes everything filed under it.
///
/// A pending row belongs to the thread of the agent it is for. Its key is
/// the id of the entry it becomes, which like every entry id is unique
/// within its session and not beyond.
///
/// `tasks` holds the task records of the running commands that outlive
/// the process (see [`TaskRow`]). A row is deleted when its task ends, in
/// the transaction that saves the task notification. `sessions.last_task`
/// is the highest task id the session minted, so ids are not used twice
/// and `(session, id)` is a task's key.
const INITIAL_001: &str = r#"
CREATE TABLE sessions (
    id INTEGER PRIMARY KEY,
    project TEXT NOT NULL CHECK (project <> ''),
    name TEXT NOT NULL,
    label TEXT CHECK (label IS NULL OR length(CAST(label AS BLOB)) BETWEEN 1 AND 80),
    archived INTEGER NOT NULL DEFAULT 0 CHECK (archived IN (0, 1)),
    last_seq INTEGER NOT NULL DEFAULT 0 CHECK (last_seq >= 0),
    last_task INTEGER NOT NULL DEFAULT 0 CHECK (last_task >= 0),
    updated_at TEXT NOT NULL,
    UNIQUE (project, name)
) STRICT;
-- Listing the labelled and archived sessions reads only those rows.
CREATE INDEX sessions_annotated ON sessions(project, name)
    WHERE label IS NOT NULL OR archived;
CREATE TABLE threads (
    id INTEGER PRIMARY KEY,
    session INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('main', 'sub')),
    agent INTEGER CHECK ((kind = 'sub') = (agent IS NOT NULL)),
    parent INTEGER REFERENCES threads(id) ON DELETE CASCADE,
    forked_at INTEGER CHECK ((parent IS NULL) = (forked_at IS NULL)),
    last_seq INTEGER NOT NULL,
    turn TEXT NOT NULL DEFAULT 'idle' CHECK (turn IN ('idle', 'working'))
) STRICT;
CREATE INDEX threads_session ON threads(session);
CREATE TABLE entries (
    session INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL CHECK (seq > 0),
    thread INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    entry_id TEXT NOT NULL,
    type TEXT NOT NULL,
    entry TEXT NOT NULL,
    PRIMARY KEY (session, seq),
    UNIQUE (session, entry_id)
) STRICT;
CREATE INDEX entries_path ON entries(thread, seq, type);
CREATE INDEX entries_usage ON entries(
    thread, seq,
    json_extract(entry, '$.message.usage'),
    json_extract(entry, '$.message.stop_reason')
) WHERE type = 'assistant';
CREATE INDEX entries_state ON entries(thread, seq)
    WHERE type IN ('system_prompt', 'context', 'model_change', 'oracle_model_change', 'oracle_thinking_change', 'oracle_speed_change', 'oracle_verbosity_change', 'account_change', 'thinking_change', 'speed_change', 'verbosity_change', 'env_change', 'goal_change', 'code_mode_store', 'sub_agent_spawn', 'sub_agent_end', 'compaction_failed', 'compaction');
CREATE TABLE pending (
    session INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    id TEXT NOT NULL,
    thread INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('steering', 'follow_up', 'task_notification')),
    body TEXT NOT NULL,
    PRIMARY KEY (session, id)
) STRICT;
CREATE TABLE tasks (
    session INTEGER NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    id INTEGER NOT NULL CHECK (id > 0),
    thread INTEGER NOT NULL REFERENCES threads(id) ON DELETE CASCADE,
    call_id TEXT NOT NULL,
    command TEXT NOT NULL,
    pid INTEGER NOT NULL CHECK (pid > 1),
    process_start INTEGER NOT NULL,
    output_path TEXT NOT NULL,
    exit_path TEXT NOT NULL,
    started_at TEXT NOT NULL,
    PRIMARY KEY (session, id)
) STRICT;
"#;

/// The id SQLite assigned a stored thread.
pub(crate) type ThreadId = i64;

/// One stored thread, as [`SqliteStore::threads`] reads it.
pub(crate) struct ThreadRow {
    pub id: ThreadId,
    pub owner: Owner,
    /// The entry it continues from: its thread and position.
    pub parent: Option<(ThreadId, u64)>,
    /// Position and entry id of its last entry.
    pub last_seq: u64,
    pub last_entry_id: String,
}

/// The agent a pending row or a task record is for, naming the thread it
/// belongs to.
#[derive(Clone, Copy, Debug)]
pub(crate) enum WorkOwner {
    /// The main agent, on the main thread its writer is on.
    Main(ThreadId),
    /// A sub-agent, on its run of the branch its writer is on.
    Sub(ThreadId),
}

/// What [`SqliteStore::create_thread`] writes.
pub(crate) struct Creation<'a> {
    /// The label and archive marker of a session to create. `None` for a
    /// session the store holds.
    pub session: Option<&'a SessionAnnotations>,
    /// The entry the thread continues from. `None` for the
    /// session's first thread.
    pub from: Option<&'a str>,
    /// The entries that precede the thread's first entry.
    pub entries: &'a [&'a LogEntry],
    /// The input the thread is created for.
    pub input: Option<&'a PendingRow>,
    /// The state its turn starts in.
    pub turn: crate::TurnState,
}

/// What [`SqliteStore::create_thread`] created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Created {
    pub thread: ThreadId,
    /// The entry it continues from: its thread and position.
    pub from: Option<(ThreadId, u64)>,
    /// Where the entries landed.
    pub appended: Appended,
}

/// How the store filed an entry (see [`place`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landing {
    /// It became the last entry of a thread that existed, which
    /// may have held none before.
    Continues,
    /// A meta entry, filed in the first thread without becoming its
    /// last entry: the next entry can still continue the entry before.
    Beside,
    /// It started its thread, which continues from the entry
    /// `from` names by thread and position. `None` for the
    /// thread that starts the session.
    Starts { from: Option<(ThreadId, u64)> },
}

/// Where the store put an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Landed {
    /// The position the store assigned.
    pub seq: u64,
    pub thread: ThreadId,
    pub landing: Landing,
}

/// What a committed batch left behind.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Appended {
    pub metadata: SessionMetadata,
    /// Where each entry of the batch landed, in the batch's order.
    pub landed: Vec<Landed>,
    /// The main thread the batch appended to last. `None` when it
    /// appended to none.
    pub head_thread: Option<ThreadId>,
}

/// A stretch of one thread: its entries at the positions
/// after `after`, up to and including `upto`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub thread: ThreadId,
    pub after: u64,
    pub upto: u64,
}

/// What [`SqliteStore::walk`] reports of one entry without reading it.
pub(crate) struct Shape<'a> {
    pub seq: u64,
    pub thread: ThreadId,
    /// The row type: what kind of message it is, or the entry's own type
    /// for a state entry.
    pub kind: &'a str,
}

impl Shape<'_> {
    pub(crate) fn class(&self) -> Class {
        Class::of_type(self.kind)
    }
}

/// Where a stored entry sits, as [`SqliteStore::locate`] reads it.
pub(crate) struct Located {
    pub seq: u64,
    pub thread: ThreadId,
    pub parent_id: Option<String>,
}

/// Something accepted that its agent has not yet taken: an input or a
/// finished task's notification. `id` is the id of the entry it becomes.
/// Appending an entry with that id removes the row in the same
/// transaction, so delivery is neither lost nor repeated.
/// A row belongs to the thread of the agent it is for (see
/// [`WorkOwner`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingRow {
    pub id: String,
    /// `steering`, `follow_up` or `task_notification`.
    pub kind: String,
    /// An input's content (see [`crate::pending`] for its two forms), or
    /// the task notification's serialized message.
    pub body: String,
}

/// A background task: a running command that does not end with the
/// process that started it. The row is what a later process finds the
/// command by, and it exists exactly while nothing durable says how the
/// command ended. It belongs to the thread of the agent that started
/// the command, as a pending row does, and the task notification is saved there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TaskRow {
    /// The task id users see, unique in the session.
    pub id: u64,
    /// The tool call that launched the command.
    pub call_id: String,
    pub command: String,
    /// The process that runs the command, which leads its process group.
    pub pid: u32,
    /// When that process started, on the system's own clock. A process
    /// with this pid and another start time is not the task.
    pub process_start: u64,
    /// The file the command's output is written to.
    pub output_path: String,
    /// The file the command's exit status is written to when it ends.
    pub exit_path: String,
    pub started_at: DateTime<Utc>,
}

/// Session-scoped display state, independent of branch selection and history.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionAnnotations {
    pub tag: Option<String>,
    pub archived: bool,
}

/// What a listing shows of a session's messages, read without their bodies.
pub(crate) struct MessageSummary {
    pub count: usize,
    pub last_at: Option<DateTime<Utc>>,
}

/// Storage failures are distinct from validation failures of a session's entries.
#[derive(Debug, Error)]
pub enum SqliteError {
    #[error("SQLite error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "the session database has format {0}, which this build cannot read. \
         Use the build that wrote it, or move the database and its `-wal` and `-shm` \
         files aside to start a new one"
    )]
    Version(i64),
    #[error(
        "the session database was written by a build whose storage format differs \
         from this one's. Use that build, or move the database and its `-wal` and \
         `-shm` files aside to start a new one"
    )]
    Format,
    #[error(
        "{0} is left over from a session database that is no longer there. \
         Restore that database, or move the file aside to start a new one"
    )]
    Leftover(std::path::PathBuf),
    #[error("invalid session storage data: {0}")]
    Invalid(String),
    #[error("the session is closed, so nothing more is written to it")]
    Closed,
    #[error("{0:?} is not a session id")]
    InvalidSessionId(String),
    #[error("session {0:?} already exists")]
    SessionExists(String),
    #[error("session {0:?} does not exist")]
    SessionNotFound(String),
    #[error("entry {entry_id:?} already exists in session {session_id:?}")]
    EntryExists {
        session_id: String,
        entry_id: String,
    },
    #[error("session database was opened read-only")]
    ReadOnly,
    /// An invariant was broken: an agent's work has a thread from
    /// the first input accepted for it.
    #[error("{0} has no thread to hold its work")]
    NoThread(String),
}

impl SqliteError {
    /// Another connection held the database past the busy timeout, or a
    /// lock inside it was contended. Nothing of the refused statement or
    /// transaction was applied, so it can be tried again. This is the one
    /// test for "busy" every caller uses.
    pub(crate) fn is_busy(&self) -> bool {
        use rusqlite::ErrorCode::{DatabaseBusy, DatabaseLocked};
        matches!(
            self,
            Self::Sql(rusqlite::Error::SqliteFailure(error, _))
                if matches!(error.code, DatabaseBusy | DatabaseLocked)
        )
    }
}

/// Metadata updated in the same transaction as the entries it describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionMetadata {
    pub id: String,
    /// Position of the last entry, which is how many the session
    /// holds. Zero if empty.
    pub last_seq: u64,
    /// When the last batch of entries was appended, as its writer supplied
    /// it. An import can preserve the source's modification time.
    pub updated_at: DateTime<Utc>,
}

/// One SQLite connection bound to an immutable project scope.
///
/// Mutations are atomic and never replace existing entries. One host
/// writes a session at a time: the caller holds the session lock across
/// its read and append lifecycle, and the store relies on it. SQLite
/// serializes transactions across sessions, and a statement waits for
/// another connection's lock up to the busy timeout, five seconds unless
/// [`Self::set_busy_timeout`] says otherwise.
pub struct SqliteStore {
    connection: Connection,
    project_id: String,
    read_only: bool,
    /// The session this handle resolved last, by name, with its row id. A
    /// session keeps its row for as long as it exists, so the name is
    /// looked up once per handle and not once per call.
    resolved: std::cell::RefCell<Option<(String, i64)>>,
}

impl SqliteStore {
    /// A store in memory, for running a write's constraints without creating
    /// persistent storage.
    pub(crate) fn in_memory(project_id: &str) -> Result<Self, SqliteError> {
        validate_project_id(project_id)?;
        let mut connection = Connection::open_in_memory()?;
        migrate(&mut connection)?;
        configure(&connection)?;
        Ok(Self {
            connection,
            project_id: project_id.into(),
            read_only: false,
            resolved: Default::default(),
        })
    }

    /// Open or create a database bound to a nonempty project identity, and
    /// apply any migration it lacks. Creates parent directories and uses WAL,
    /// NORMAL synchronous writes and foreign keys.
    pub fn open_project(database_path: &Path, project_id: &str) -> Result<Self, SqliteError> {
        validate_project_id(project_id)?;
        let directory = database_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(directory)?;
        match std::fs::metadata(database_path) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                refuse_leftovers(database_path)?;
                install_database(directory, database_path)?;
            }
            Err(error) => return Err(error.into()),
        }
        let mut connection =
            Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        // Even the schema read can race another connection's WAL checkpoint.
        connection.busy_timeout(BUSY_TIMEOUT)?;
        if !pending_migrations(&connection)?.is_empty() {
            migrate(&mut connection)?;
        }
        configure(&connection)?;
        // A no-op on a database this code created. It switches one that
        // began as an empty file someone else left at the path.
        let mode: String = connection.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(SqliteError::Invalid(format!(
                "expected WAL journal mode, got {mode}"
            )));
        }
        Ok(Self {
            connection,
            project_id: project_id.to_owned(),
            read_only: false,
            resolved: Default::default(),
        })
    }

    /// Open an existing database bound to a nonempty project identity, for
    /// reading. Creates no database or directory. SQLite may need WAL
    /// sidecars. A database that lacks a migration is migrated first, which
    /// writes: a reader may be the first process to meet it after an upgrade.
    pub fn open_project_read_only(
        database_path: &Path,
        project_id: &str,
    ) -> Result<Self, SqliteError> {
        validate_project_id(project_id)?;
        let open = || -> Result<Connection, SqliteError> {
            let connection =
                Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            connection.busy_timeout(BUSY_TIMEOUT)?;
            Ok(connection)
        };
        let mut connection = open()?;
        if !pending_migrations(&connection)?.is_empty() {
            drop(connection);
            let mut writer =
                Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
            writer.busy_timeout(BUSY_TIMEOUT)?;
            migrate(&mut writer)?;
            drop(writer);
            connection = open()?;
        }
        configure(&connection)?;
        Ok(Self {
            connection,
            project_id: project_id.to_owned(),
            read_only: true,
            resolved: Default::default(),
        })
    }

    /// Atomically import a whole session with its label and archive
    /// marker, or change nothing. The entries are filed as
    /// [`Self::append`] files them. Every thread starts idle and no
    /// pending row is retired.
    ///
    /// A session identity that exists is refused even when its entries
    /// match. Entries keep their serialized fields and input order. No
    /// normalization or repair occurs.
    pub(crate) fn create_session(
        &mut self,
        session_id: &str,
        entries: &[&LogEntry],
        updated_at: DateTime<Utc>,
        annotations: &SessionAnnotations,
    ) -> Result<Appended, SqliteError> {
        validate_session_id(session_id)?;
        validate_tag(annotations.tag.as_deref())?;
        self.ensure_writable()?;
        let stored = serialize(entries)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if session_row(&tx, &self.project_id, session_id)?.is_some() {
            return Err(SqliteError::SessionExists(session_id.to_owned()));
        }
        tx.execute(
            "INSERT INTO sessions (project, name, label, archived, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                self.project_id,
                session_id,
                annotations.tag,
                annotations.archived,
                timestamp(updated_at)
            ],
        )?;
        let session = tx.last_insert_rowid();
        let initial = SessionMetadata {
            id: session_id.to_owned(),
            last_seq: 0,
            updated_at,
        };
        let result = insert_batch(
            &tx, session, initial, entries, &stored, updated_at, false, None,
        )?;
        tx.commit()?;
        *self.resolved.borrow_mut() = Some((session_id.to_owned(), session));
        Ok(result)
    }

    /// Atomically create a main thread for its first input, or
    /// change nothing: the session when the store does not hold it, the
    /// thread, the entries that precede its first entry, the input
    /// and the state of its turn (see [`Creation`]). The entries are filed
    /// as an append to the new thread files them (see
    /// [`Self::append`]).
    ///
    /// A session to create that exists is refused, and so is a `from` the
    /// session does not hold.
    pub(crate) fn create_thread(
        &mut self,
        session_id: &str,
        creation: &Creation<'_>,
        updated_at: DateTime<Utc>,
    ) -> Result<Created, SqliteError> {
        validate_session_id(session_id)?;
        self.ensure_writable()?;
        let stored = serialize(creation.entries)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = stored_session(&tx, &self.project_id, session_id)?;
        let (session, metadata) = match (found, creation.session) {
            (Some(_), Some(_)) => return Err(SqliteError::SessionExists(session_id.to_owned())),
            (Some(found), None) => found,
            (None, None) => return Err(SqliteError::SessionNotFound(session_id.to_owned())),
            (None, Some(annotations)) => {
                validate_tag(annotations.tag.as_deref())?;
                tx.execute(
                    "INSERT INTO sessions (project, name, label, archived, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        self.project_id,
                        session_id,
                        annotations.tag,
                        annotations.archived,
                        timestamp(updated_at)
                    ],
                )?;
                let metadata = SessionMetadata {
                    id: session_id.to_owned(),
                    last_seq: 0,
                    updated_at,
                };
                (tx.last_insert_rowid(), metadata)
            }
        };
        let from: Option<(ThreadId, u64)> = match creation.from {
            None => None,
            Some(entry) => {
                let found = tx
                    .query_row(
                        "SELECT thread, seq FROM entries
                         WHERE session = ?1 AND entry_id = ?2",
                        params![session, entry],
                        |row| Ok((row.get(0)?, unsigned_column(row, 1)?)),
                    )
                    .optional()?;
                Some(found.ok_or_else(|| {
                    SqliteError::Invalid(format!(
                        "session {session_id:?} holds no entry {entry:?} to continue from"
                    ))
                })?)
            }
        };
        // An empty thread ends where it starts.
        tx.execute(
            "INSERT INTO threads (session, kind, parent, forked_at, last_seq, turn)
             VALUES (?1, 'main', ?2, ?3, ?4, ?5)",
            params![
                session,
                from.map(|(thread, _)| thread),
                from.map(|(_, at)| integer(at)).transpose()?,
                integer(from.map_or(0, |(_, at)| at))?,
                creation.turn.as_str(),
            ],
        )?;
        let thread = tx.last_insert_rowid();
        let appended = insert_batch(
            &tx,
            session,
            metadata,
            creation.entries,
            &stored,
            updated_at,
            true,
            Some(thread),
        )?;
        if let Some(input) = creation.input {
            insert_pending(&tx, session, WorkOwner::Main(thread), input)?;
        }
        tx.commit()?;
        *self.resolved.borrow_mut() = Some((session_id.to_owned(), session));
        Ok(Created {
            thread,
            from,
            appended,
        })
    }

    /// Append entries to a stored session in one transaction, or change
    /// nothing. Each entry is filed where [`place`] puts it, after the
    /// session's last entry, and the answer says where each one landed.
    /// An entry that carries the id of a pending row delivers it: the
    /// row is removed in the same transaction.
    ///
    /// An id the session already holds is [`SqliteError::EntryExists`].
    ///
    /// `on` is the main thread the writer's main agent is on. While
    /// it holds no entry, the entry that continues from where it starts
    /// is its first (see [`place`]).
    ///
    /// A sub-agent's thread starts with its spawn entry, appended
    /// by the process that then runs it, so it is stored as working with
    /// that entry.
    pub(crate) fn append(
        &self,
        session_id: &str,
        entries: &[&LogEntry],
        updated_at: DateTime<Utc>,
        on: Option<ThreadId>,
    ) -> Result<Appended, SqliteError> {
        let stored = serialize(entries)?;
        let (tx, session) = self.write(session_id)?;
        let found = tx
            .prepare_cached("SELECT id, name, last_seq, updated_at FROM sessions WHERE id = ?1")?
            .query_row([session], metadata_row)
            .optional()?;
        let (_, metadata) =
            found.ok_or_else(|| SqliteError::SessionNotFound(session_id.to_owned()))?;
        let result = insert_batch(
            &tx, session, metadata, entries, &stored, updated_at, true, on,
        )?;
        tx.commit()?;
        Ok(result)
    }

    /// Whether the session exists, including a session with no entries.
    pub fn contains_session(&self, session_id: &str) -> Result<bool, SqliteError> {
        Ok(self.session(session_id)?.is_some())
    }

    /// Return metadata without decoding any entries. Missing sessions yield None.
    pub fn session_metadata(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionMetadata>, SqliteError> {
        validate_session_id(session_id)?;
        Ok(stored_session(&self.connection, &self.project_id, session_id)?.map(|(_, m)| m))
    }

    /// List metadata only, sorted by session identity. No entries are decoded.
    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>, SqliteError> {
        let mut stmt = self.connection.prepare_cached(
            "SELECT id, name, last_seq, updated_at FROM sessions
             WHERE project = ?1 ORDER BY name",
        )?;
        Ok(stmt
            .query_map([&self.project_id], |row| metadata_row(row).map(|(_, m)| m))?
            .collect::<Result<_, _>>()?)
    }

    /// Explicit cross-project catalog discovery, without reading entries.
    /// Session operations on this handle remain bound to its own project.
    pub fn project_ids(&self) -> Result<Vec<String>, SqliteError> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT DISTINCT project FROM sessions ORDER BY project")?;
        Ok(statement
            .query_map([], |row| row.get(0))?
            .collect::<Result<_, _>>()?)
    }

    /// Read annotations without materializing history. Unset fields use defaults.
    pub fn annotations(&self, session_id: &str) -> Result<SessionAnnotations, SqliteError> {
        validate_session_id(session_id)?;
        let annotations = self
            .connection
            .query_row(
                "SELECT label, archived FROM sessions WHERE project = ?1 AND name = ?2",
                params![self.project_id, session_id],
                |row| {
                    Ok(SessionAnnotations {
                        tag: row.get(0)?,
                        archived: row.get(1)?,
                    })
                },
            )
            .optional()?
            .unwrap_or_default();
        validate_tag(annotations.tag.as_deref())?;
        Ok(annotations)
    }

    /// Every labelled or archived session of this project, plus the
    /// sessions among `legacy_ids`, at one SQL snapshot. A legacy sidecar of
    /// a session this database holds must not supply its state, also when
    /// the database holds none for it.
    pub(crate) fn annotation_snapshot<'a>(
        &self,
        legacy_ids: impl Iterator<Item = &'a str>,
    ) -> Result<Vec<(String, SessionAnnotations)>, SqliteError> {
        let transaction = self.connection.unchecked_transaction()?;
        let mut statement = transaction.prepare_cached(
            "SELECT name, label, archived FROM sessions
             WHERE project = ?1 AND (label IS NOT NULL OR archived) ORDER BY name",
        )?;
        let mut rows = std::collections::BTreeMap::new();
        for row in statement.query_map([&self.project_id], |row| {
            let annotations = SessionAnnotations {
                tag: row.get(1)?,
                archived: row.get(2)?,
            };
            Ok((row.get::<_, String>(0)?, annotations))
        })? {
            let (id, annotations) = row?;
            validate_session_id(&id)?;
            validate_tag(annotations.tag.as_deref())?;
            rows.insert(id, annotations);
        }
        for id in legacy_ids {
            if !rows.contains_key(id) && self.contains_session(id)? {
                rows.insert(id.to_owned(), SessionAnnotations::default());
            }
        }
        // Dropping the read transaction releases its snapshot without writes.
        Ok(rows.into_iter().collect())
    }

    /// Set one normalized label of a stored session, without changing
    /// archiving or the update time. The caller holds the session writer
    /// lock.
    pub fn set_tag(&self, session_id: &str, tag: Option<&str>) -> Result<(), SqliteError> {
        validate_tag(tag)?;
        self.annotate(session_id, "label", &tag)
    }

    /// Set archiving of a stored session, without changing the label or the
    /// update time. The caller holds the session writer lock.
    pub fn set_archived(&self, session_id: &str, archived: bool) -> Result<(), SqliteError> {
        self.annotate(session_id, "archived", &archived)
    }

    fn annotate(
        &self,
        session_id: &str,
        column: &str,
        value: &dyn rusqlite::ToSql,
    ) -> Result<(), SqliteError> {
        validate_session_id(session_id)?;
        self.ensure_writable()?;
        let changed = self.connection.execute(
            &format!("UPDATE sessions SET {column} = ?3 WHERE project = ?1 AND name = ?2"),
            params![self.project_id, session_id, value],
        )?;
        if changed == 0 {
            return Err(SqliteError::SessionNotFound(session_id.to_owned()));
        }
        Ok(())
    }

    /// Explicitly materialize the complete session, in append order.
    pub fn full_entries(&self, session_id: &str) -> Result<Vec<LogEntry>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(Vec::new());
        };
        self.query_entries(
            "SELECT entry FROM entries WHERE session = ?1 ORDER BY seq",
            params![session],
        )
    }

    /// The session's threads and every entry with the thread
    /// it belongs to, in append order, at one snapshot.
    #[allow(clippy::type_complexity)]
    pub(crate) fn whole(
        &self,
        session_id: &str,
    ) -> Result<(Vec<ThreadRow>, Vec<(ThreadId, LogEntry)>), SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok((Vec::new(), Vec::new()));
        };
        // Dropping the read transaction releases its snapshot without writes.
        let _snapshot = self.connection.unchecked_transaction()?;
        let threads = self.threads(session_id)?;
        let mut stmt = self
            .connection
            .prepare_cached("SELECT thread, entry FROM entries WHERE session = ?1 ORDER BY seq")?;
        let mut rows = stmt.query(params![session])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            let text = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
            entries.push((row.get(0)?, parse_entry(text)?));
        }
        Ok((threads, entries))
    }

    /// The summed length of the session's entries as stored, in bytes. Read
    /// from the row headers, without any entry.
    pub(crate) fn entry_bytes(&self, session_id: &str) -> Result<u64, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(0);
        };
        Ok(self.connection.query_row(
            "SELECT COALESCE(SUM(octet_length(entry)), 0) FROM entries WHERE session = ?1",
            [session],
            |row| unsigned_column(row, 0),
        )?)
    }

    /// The session's threads, in the order they started. One that
    /// holds no entry ends at the entry it continues from, and the
    /// session's first at position `0` with an empty entry id.
    pub(crate) fn threads(&self, session_id: &str) -> Result<Vec<ThreadRow>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(Vec::new());
        };
        let mut stmt = self.connection.prepare_cached(
            "SELECT th.id, th.kind, th.agent, th.parent, th.forked_at, th.last_seq, e.entry_id
             FROM threads th INDEXED BY threads_session
             LEFT JOIN entries e ON e.session = th.session AND e.seq = th.last_seq
             WHERE th.session = ?1 ORDER BY th.id",
        )?;
        let mut rows = stmt.query(params![session])?;
        let mut threads = Vec::new();
        while let Some(row) = rows.next()? {
            let kind = row.get_ref(1)?.as_str().unwrap_or_default();
            let owner = match (kind, row.get::<_, Option<i64>>(2)?) {
                ("main", None) => Owner::Main,
                ("sub", Some(agent)) => Owner::Sub(usize::try_from(agent).map_err(|_| {
                    SqliteError::Invalid(format!("sub-agent number {agent} is out of range"))
                })?),
                (kind, _) => {
                    return Err(SqliteError::Invalid(format!(
                        "unknown thread kind {kind:?}"
                    )));
                }
            };
            let parent = match (row.get::<_, Option<i64>>(3)?, row.get::<_, Option<i64>>(4)?) {
                (Some(thread), Some(_)) => Some((thread, unsigned_column(row, 4)?)),
                _ => None,
            };
            threads.push(ThreadRow {
                id: row.get(0)?,
                owner,
                parent,
                last_seq: unsigned_column(row, 5)?,
                last_entry_id: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
            });
        }
        Ok(threads)
    }

    /// Visit the entries of `spans` without reading them, in
    /// append order or against it, until `visit` returns `false`. `kind`
    /// keeps the rows of one type. The spans may be of any threads
    /// and may lie in any order.
    ///
    /// Each thread is read in its own order through `entries_path`,
    /// one row at a time, and the nearest row of all spans is visited
    /// next. So a walk that stops early has read near where it stopped and
    /// the near end of each span, and nowhere else.
    pub(crate) fn walk(
        &self,
        spans: &[Span],
        descending: bool,
        kind: Option<&str>,
        mut visit: impl FnMut(Shape<'_>) -> bool,
    ) -> Result<(), SqliteError> {
        let mut fetch = self.connection.prepare_cached(&walk_sql(descending))?;
        let mut next = |span: &Span| -> Result<Option<(u64, String)>, SqliteError> {
            let within = params![span.thread, bound(span.after), bound(span.upto), kind];
            let row = fetch.query_row(within, |row| Ok((unsigned_column(row, 0)?, row.get(1)?)));
            Ok(row.optional()?)
        };
        // What is left of each span that has a row left, with its next row.
        let mut fronts = Vec::with_capacity(spans.len());
        for span in spans {
            if let Some(row) = next(span)? {
                fronts.push((*span, row));
            }
        }
        loop {
            let seqs = fronts.iter().map(|(_, (seq, _))| *seq).enumerate();
            let nearest = if descending {
                seqs.max_by_key(|(_, seq)| *seq)
            } else {
                seqs.min_by_key(|(_, seq)| *seq)
            };
            let Some((index, seq)) = nearest else {
                return Ok(());
            };
            let (span, (_, kind)) = &mut fronts[index];
            let shape = Shape {
                seq,
                thread: span.thread,
                kind,
            };
            if !visit(shape) {
                return Ok(());
            }
            if descending {
                span.upto = seq - 1;
            } else {
                span.after = seq;
            }
            match next(span)? {
                Some(row) => fronts[index].1 = row,
                None => drop(fronts.swap_remove(index)),
            }
        }
    }

    /// The entries of `span`, each with its position, in append
    /// order. `state` keeps the state entries and passes over the entries
    /// between them.
    pub(crate) fn entries(
        &self,
        span: Span,
        state: bool,
    ) -> Result<Vec<(u64, LogEntry)>, SqliteError> {
        let mut stmt = self.connection.prepare_cached(&entries_sql(state))?;
        let mut rows = stmt.query(params![span.thread, bound(span.after), bound(span.upto)])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            let text = row.get_ref(1)?.as_str().map_err(rusqlite::Error::from)?;
            entries.push((unsigned_column(row, 0)?, parse_entry(text)?));
        }
        Ok(entries)
    }

    /// Visit the usage each completed model response of `span` reported,
    /// with the response's position, in append order. A response
    /// that failed or was cancelled reports nothing.
    ///
    /// The usage and the stop reason are read off `entries_usage`, which
    /// holds those two fields of every response. No entry is read.
    pub(crate) fn usage(
        &self,
        span: Span,
        mut visit: impl FnMut(u64, aj_models::types::Usage),
    ) -> Result<(), SqliteError> {
        let mut stmt = self.connection.prepare_cached(USAGE_SQL)?;
        let mut rows = stmt.query(params![span.thread, bound(span.after), bound(span.upto)])?;
        while let Some(row) = rows.next()? {
            let (Some(usage), Some(stop)) = (
                row.get_ref(1)?
                    .as_str_or_null()
                    .map_err(rusqlite::Error::from)?,
                row.get_ref(2)?
                    .as_str_or_null()
                    .map_err(rusqlite::Error::from)?,
            ) else {
                continue;
            };
            let stop: serde::de::value::StrDeserializer<'_, serde_json::Error> =
                serde::de::IntoDeserializer::into_deserializer(stop);
            let stop: aj_models::types::StopReason = serde::Deserialize::deserialize(stop)?;
            if stop.completed() {
                visit(unsigned_column(row, 0)?, serde_json::from_str(usage)?);
            }
        }
        Ok(())
    }

    /// Where the entry with entry id `id` sits, `None` when the session
    /// holds none.
    pub(crate) fn locate(
        &self,
        session_id: &str,
        id: &str,
    ) -> Result<Option<Located>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(None);
        };
        Ok(self
            .connection
            .query_row(
                "SELECT seq, thread, json_extract(entry, '$.parent_id') FROM entries
                 WHERE session = ?1 AND entry_id = ?2",
                params![session, id],
                |row| {
                    Ok(Located {
                        seq: unsigned_column(row, 0)?,
                        thread: row.get(1)?,
                        parent_id: row.get(2)?,
                    })
                },
            )
            .optional()?)
    }

    /// The entries at `positions`, in append order.
    pub(crate) fn entries_at(
        &self,
        session_id: &str,
        positions: &[u64],
    ) -> Result<Vec<LogEntry>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(Vec::new());
        };
        let positions = serde_json::to_string(positions).expect("integers serialize");
        self.query_entries(
            "SELECT entry FROM entries WHERE session = ?1
             AND seq IN (SELECT value FROM json_each(?2)) ORDER BY seq",
            params![session, positions],
        )
    }

    /// How many messages the agents exchanged in the session, counted from
    /// an index, and when the last one was written, read from its entry.
    pub(crate) fn message_summary(&self, session_id: &str) -> Result<MessageSummary, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(MessageSummary {
                count: 0,
                last_at: None,
            });
        };
        let count = self.connection.query_row(
            &format!(
                "SELECT COUNT(*) FROM threads th
                 JOIN entries e INDEXED BY entries_path ON e.thread = th.id
                 WHERE th.session = ?1 AND e.type IN {}",
                *MESSAGES
            ),
            params![session],
            |row| unsigned_column(row, 0),
        )?;
        // The newest rows are walked until the first message among them.
        let last: Option<String> = self
            .connection
            .query_row(
                &format!(
                    "SELECT json_extract(entry, '$.timestamp') FROM entries
                     WHERE session = ?1 AND type IN {} ORDER BY seq DESC LIMIT 1",
                    *MESSAGES
                ),
                params![session],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        Ok(MessageSummary {
            count: usize::try_from(count)
                .map_err(|_| SqliteError::Invalid("message count exceeds memory".into()))?,
            last_at: last
                .and_then(|text| DateTime::parse_from_rfc3339(&text).ok())
                .map(|time| time.with_timezone(&Utc)),
        })
    }

    /// Visit the session's entries of one type in append order, until
    /// `visit` returns `false`. `main_only` leaves out sub-agent
    /// threads. Rows of other types are passed over without their
    /// entries, and a scan that stops early has read no row far past
    /// where it stopped (see [`Self::walk`]).
    pub(crate) fn scan(
        &self,
        session_id: &str,
        kind: &str,
        main_only: bool,
        mut visit: impl FnMut(LogEntry) -> bool,
    ) -> Result<(), SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(());
        };
        // One snapshot, so concurrent appends cannot keep the scan alive.
        let _snapshot = self.connection.unchecked_transaction()?;
        // A thread's entries come after the one it continues
        // from, which is what lets the walk leave it alone until then.
        let mut whole = self.connection.prepare_cached(
            "SELECT id, COALESCE(forked_at, 0) FROM threads
             INDEXED BY threads_session
             WHERE session = ?1 AND (?2 = 0 OR kind = 'main')",
        )?;
        let spans = whole.query_map(params![session, main_only], |row| {
            Ok(Span {
                thread: row.get(0)?,
                after: unsigned_column(row, 1)?,
                upto: u64::MAX,
            })
        })?;
        let spans: Vec<Span> = spans.collect::<Result<_, _>>()?;
        let mut read_entry = self
            .connection
            .prepare_cached("SELECT entry FROM entries WHERE session = ?1 AND seq = ?2")?;
        let mut failed = None;
        self.walk(&spans, false, Some(kind), |row| {
            let read = read_entry.query_row(params![session, bound(row.seq)], |row| {
                row.get::<_, String>(0)
            });
            let parsed = read.map_err(SqliteError::from);
            match parsed.and_then(|text| parse_entry(&text)) {
                Ok(entry) => visit(entry),
                Err(error) => {
                    failed = Some(error);
                    false
                }
            }
        })?;
        failed.map_or(Ok(()), Err)
    }

    /// The pending rows of one thread, in the order they were saved.
    pub(crate) fn pending(
        &self,
        session_id: &str,
        thread: ThreadId,
    ) -> Result<Vec<PendingRow>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(Vec::new());
        };
        let mut statement = self.connection.prepare_cached(
            "SELECT id, kind, body FROM pending
             WHERE session = ?1 AND thread = ?2 ORDER BY rowid",
        )?;
        let rows = statement.query_map(params![session, thread], |row| {
            Ok(PendingRow {
                id: row.get(0)?,
                kind: row.get(1)?,
                body: row.get(2)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Save a pending row under its id, in its owner's thread (see
    /// [`WorkOwner`]). A copy saved earlier keeps its place in the order
    /// and its thread, and takes the new body. A row whose entry the
    /// session already holds was delivered while the caller was saving
    /// it, so it is not saved again.
    ///
    /// Returning `Ok` means the row is saved or was delivered. An owner
    /// that names no thread of the session is an error.
    pub(crate) fn save_pending(
        &self,
        session_id: &str,
        owner: WorkOwner,
        row: &PendingRow,
    ) -> Result<(), SqliteError> {
        let (tx, session) = self.write(session_id)?;
        insert_pending(&tx, session, owner, row)?;
        tx.commit()?;
        Ok(())
    }

    /// Drop one pending row that will not be delivered.
    pub(crate) fn delete_pending(&self, session_id: &str, id: &str) -> Result<(), SqliteError> {
        let (tx, session) = self.write(session_id)?;
        tx.execute(
            "DELETE FROM pending WHERE session = ?1 AND id = ?2",
            params![session, id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// The highest task id the session minted. Zero for a session that is
    /// not stored.
    pub(crate) fn last_task(&self, session_id: &str) -> Result<u64, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(0);
        };
        Ok(self.connection.query_row(
            "SELECT last_task FROM sessions WHERE id = ?1",
            [session],
            |row| unsigned_column(row, 0),
        )?)
    }

    /// Record that task id `id` is taken.
    pub(crate) fn reserve_task(&self, session_id: &str, id: u64) -> Result<(), SqliteError> {
        let (tx, session) = self.write(session_id)?;
        reserve_task(&tx, session, id)?;
        tx.commit()?;
        Ok(())
    }

    /// Record a running task in its owner's thread, chosen as
    /// [`Self::save_pending`] chooses it, and that its id is taken. An owner
    /// that names no thread of the session and an id the session
    /// already has a task under are errors, and record nothing.
    pub(crate) fn insert_task(
        &self,
        session_id: &str,
        owner: WorkOwner,
        task: &TaskRow,
    ) -> Result<(), SqliteError> {
        let (tx, session) = self.write(session_id)?;
        let Some(thread) = owner_thread(&tx, session, owner)? else {
            return Err(SqliteError::NoThread(format!(
                "task {} of call {}",
                task.id, task.call_id
            )));
        };
        tx.execute(
            "INSERT INTO tasks (session, id, thread, call_id, command, pid,
                                process_start, output_path, exit_path, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                session,
                signed(task.id)?,
                thread,
                task.call_id,
                task.command,
                task.pid,
                signed(task.process_start)?,
                task.output_path,
                task.exit_path,
                timestamp(task.started_at),
            ],
        )?;
        reserve_task(&tx, session, task.id)?;
        tx.commit()?;
        Ok(())
    }

    /// End task `id`: delete its row and save its notification `row` in
    /// one transaction, so the task is either still recorded or has its
    /// notification saved, never neither and never both. The notification
    /// goes to the thread the task was recorded in. A task without a row
    /// was not recorded, and its notification is saved as
    /// [`Self::save_pending`] saves it.
    pub(crate) fn finish_task(
        &self,
        session_id: &str,
        owner: WorkOwner,
        id: u64,
        row: &PendingRow,
    ) -> Result<(), SqliteError> {
        let (tx, session) = self.write(session_id)?;
        let recorded: Option<ThreadId> = tx
            .query_row(
                "DELETE FROM tasks WHERE session = ?1 AND id = ?2 RETURNING thread",
                params![session, signed(id)?],
                |row| row.get(0),
            )
            .optional()?;
        match recorded {
            Some(thread) => {
                tx.execute(
                    "INSERT INTO pending (session, id, thread, kind, body)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![session, row.id, thread, row.kind, row.body],
                )?;
            }
            None => insert_pending(&tx, session, owner, row)?,
        }
        tx.commit()?;
        Ok(())
    }

    /// The running tasks recorded in one thread, in id order.
    pub(crate) fn tasks(
        &self,
        session_id: &str,
        thread: ThreadId,
    ) -> Result<Vec<TaskRow>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(Vec::new());
        };
        let mut statement = self.connection.prepare_cached(
            "SELECT id, call_id, command, pid, process_start, output_path, exit_path, started_at
             FROM tasks WHERE session = ?1 AND thread = ?2 ORDER BY id",
        )?;
        let rows = statement.query_map(params![session, thread], |row| {
            Ok(TaskRow {
                id: unsigned_column(row, 0)?,
                call_id: row.get(1)?,
                command: row.get(2)?,
                pid: row.get(3)?,
                process_start: unsigned_column(row, 4)?,
                output_path: row.get(5)?,
                exit_path: row.get(6)?,
                started_at: time_column(row, 7)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The session's sub-agent threads that hold a task record
    /// or a pending row or whose turn is stored as working, each with
    /// whether it is.
    pub(crate) fn sub_threads_with_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<(ThreadId, bool)>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(Vec::new());
        };
        let mut statement = self.connection.prepare_cached(
            "SELECT th.id, th.turn = 'working' FROM threads th
             INDEXED BY threads_session
             WHERE th.session = ?1 AND th.kind = 'sub' AND (
                 th.turn = 'working'
                 OR EXISTS (SELECT 1 FROM tasks t
                            WHERE t.session = th.session AND t.thread = th.id)
                 OR EXISTS (SELECT 1 FROM pending p
                            WHERE p.session = th.session AND p.thread = th.id))
             ORDER BY th.id",
        )?;
        let rows = statement.query_map([session], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The main thread that holds the session's newest pending row,
    /// `None` when none holds any. A reopen lands there: the row was
    /// accepted for the thread its host was on.
    pub(crate) fn main_with_pending(
        &self,
        session_id: &str,
    ) -> Result<Option<ThreadId>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(None);
        };
        let found = self.connection.query_row(
            "SELECT p.thread FROM pending p
             JOIN threads th ON th.id = p.thread AND th.kind = 'main'
             WHERE p.session = ?1 ORDER BY p.rowid DESC LIMIT 1",
            [session],
            |row| row.get(0),
        );
        Ok(found.optional()?)
    }

    /// The stored turn state of one thread.
    pub(crate) fn turn(
        &self,
        session_id: &str,
        thread: ThreadId,
    ) -> Result<crate::TurnState, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(crate::TurnState::Idle);
        };
        let turn: String = self.connection.query_row(
            "SELECT turn FROM threads WHERE session = ?1 AND id = ?2",
            params![session, thread],
            |row| row.get(0),
        )?;
        crate::TurnState::parse(&turn)
            .ok_or_else(|| SqliteError::Invalid(format!("unknown turn state {turn:?}")))
    }

    /// Record the turn state of one of the session's threads.
    pub(crate) fn set_turn(
        &self,
        session_id: &str,
        thread: ThreadId,
        turn: crate::TurnState,
    ) -> Result<(), SqliteError> {
        let (tx, session) = self.write(session_id)?;
        let set = tx.execute(
            "UPDATE threads SET turn = ?3 WHERE session = ?1 AND id = ?2",
            params![session, thread, turn.as_str()],
        )?;
        if set != 1 {
            return Err(SqliteError::Invalid(format!(
                "session {session_id:?} has no thread {thread} to hold a turn"
            )));
        }
        tx.commit()?;
        Ok(())
    }

    /// Set how long a statement waits, blocking its thread, for another
    /// connection's lock before it fails as busy.
    pub(crate) fn set_busy_timeout(&self, timeout: Duration) -> Result<(), SqliteError> {
        Ok(self.connection.busy_timeout(timeout)?)
    }

    /// Make this connection's entry inserts fail inside their transaction.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn refuse_entry_writes(&self) {
        self.connection
            .execute_batch(
                "CREATE TEMP TRIGGER IF NOT EXISTS refuse_entries BEFORE INSERT ON main.entries
                 BEGIN SELECT RAISE(ABORT, 'injected entry write failure'); END;",
            )
            .expect("install the entry fault");
    }

    /// Make every connection's entry inserts fail, or none.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn refuse_every_entry_write(&self, refuse: bool) {
        let mut sql = String::from("DROP TRIGGER IF EXISTS refuse_every_entry;");
        if refuse {
            sql.push_str(
                "CREATE TRIGGER refuse_every_entry BEFORE INSERT ON entries
                 BEGIN SELECT RAISE(ABORT, 'injected entry write failure'); END;",
            );
        }
        self.connection
            .execute_batch(&sql)
            .expect("install the entry fault");
    }

    /// Make every connection's label and archive writes fail.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn refuse_annotation_writes(&self) {
        self.connection
            .execute_batch(
                "CREATE TRIGGER refuse_annotations BEFORE UPDATE OF label, archived ON sessions
                 BEGIN SELECT RAISE(ABORT, 'injected annotation write failure'); END;",
            )
            .expect("install the annotation fault");
    }

    /// Make every connection's pending-row writes fail.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn refuse_pending_writes(&self) {
        self.connection
            .execute_batch(
                "CREATE TRIGGER refuse_pending_insert BEFORE INSERT ON pending
                 BEGIN SELECT RAISE(ABORT, 'injected pending write failure'); END;
                 CREATE TRIGGER refuse_pending_delete BEFORE DELETE ON pending
                 BEGIN SELECT RAISE(ABORT, 'injected pending write failure'); END;",
            )
            .expect("install the pending fault");
    }

    /// The id and pid of every task recorded for the session, in id order.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn all_tasks(&self, session_id: &str) -> Result<Vec<(usize, u32)>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(Vec::new());
        };
        let mut statement = self
            .connection
            .prepare("SELECT id, pid FROM tasks WHERE session = ?1 ORDER BY id")?;
        let rows = statement.query_map([session], |row| {
            let id = unsigned_column(row, 0)?;
            Ok((usize::try_from(id).unwrap_or(usize::MAX), row.get(1)?))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Everything the session stores beside its entries, one line per
    /// row: each thread with its turn, each pending row, each
    /// task record. Two sessions with equal entries and equal rows are
    /// the same to the next open.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn rows(&self, session_id: &str) -> Result<Vec<String>, SqliteError> {
        let Some(session) = self.session(session_id)? else {
            return Ok(Vec::new());
        };
        let mut lines = Vec::new();
        for sql in [
            "SELECT 'thread ' || id || ' ' || kind || ' ' || IFNULL(agent, '-')
                 || ' ' || turn FROM threads WHERE session = ?1 ORDER BY id",
            "SELECT 'pending ' || thread || ' ' || id || ' ' || kind || ' ' || body
                 FROM pending WHERE session = ?1 ORDER BY rowid",
            "SELECT 'task ' || thread || ' ' || id || ' ' || call_id || ' ' || pid
                 FROM tasks WHERE session = ?1 ORDER BY id",
        ] {
            let mut statement = self.connection.prepare(sql)?;
            let rows = statement.query_map([session], |row| row.get::<_, String>(0))?;
            for row in rows {
                lines.push(row?);
            }
        }
        Ok(lines)
    }

    /// Make every entry of sub-agent `agent`'s runs unreadable and store
    /// their turns as working, so the next open has to read them. What is
    /// left is JSON and no entry: `entries_usage` reads fields out of a
    /// response's entry, so SQLite refuses one that is not JSON at all.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn damage_sub_agent(
        &self,
        session_id: &str,
        agent: usize,
    ) -> Result<(), SqliteError> {
        let session = self.session(session_id)?;
        let session = session.ok_or_else(|| SqliteError::SessionNotFound(session_id.to_owned()))?;
        let runs = "SELECT id FROM threads WHERE session = ?1 AND kind = 'sub' AND agent = ?2";
        let agent = limit_integer(agent)?;
        self.connection.execute(
            &format!("UPDATE entries SET entry = '{{}}' WHERE thread IN ({runs})"),
            params![session, agent],
        )?;
        self.connection.execute(
            &format!("UPDATE threads SET turn = 'working' WHERE id IN ({runs})"),
            params![session, agent],
        )?;
        Ok(())
    }

    /// The pid of every task recorded in the project.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn all_task_pids(&self) -> Result<Vec<u32>, SqliteError> {
        let mut statement = self.connection.prepare(
            "SELECT t.pid FROM tasks t JOIN sessions s ON s.id = t.session
             WHERE s.project = ?1",
        )?;
        let rows = statement.query_map([&self.project_id], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Make every connection's task-row inserts or deletes fail.
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn refuse_task_writes(&self, insert: bool, delete: bool) {
        let mut sql = String::from(
            "DROP TRIGGER IF EXISTS refuse_task_insert; DROP TRIGGER IF EXISTS refuse_task_delete;",
        );
        if insert {
            sql.push_str(
                "CREATE TRIGGER refuse_task_insert BEFORE INSERT ON tasks
                 BEGIN SELECT RAISE(ABORT, 'injected task write failure'); END;",
            );
        }
        if delete {
            sql.push_str(
                "CREATE TRIGGER refuse_task_delete BEFORE DELETE ON tasks
                 BEGIN SELECT RAISE(ABORT, 'injected task write failure'); END;",
            );
        }
        self.connection
            .execute_batch(&sql)
            .expect("install the task fault");
    }

    /// Lift [`Self::refuse_pending_writes`].
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn allow_pending_writes(&self) {
        self.connection
            .execute_batch(
                "DROP TRIGGER IF EXISTS refuse_pending_insert;
                 DROP TRIGGER IF EXISTS refuse_pending_delete;",
            )
            .expect("remove the pending fault");
    }

    /// Lift [`Self::refuse_annotation_writes`].
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn allow_annotation_writes(&self) {
        self.connection
            .execute_batch("DROP TRIGGER IF EXISTS refuse_annotations;")
            .expect("remove the annotation fault");
    }

    /// The row id of a stored session.
    fn session(&self, session_id: &str) -> Result<Option<i64>, SqliteError> {
        if let Some((name, id)) = &*self.resolved.borrow()
            && name == session_id
        {
            return Ok(Some(*id));
        }
        validate_session_id(session_id)?;
        let found = session_row(&self.connection, &self.project_id, session_id)?;
        if let Some(id) = found {
            *self.resolved.borrow_mut() = Some((session_id.to_owned(), id));
        }
        Ok(found)
    }

    fn query_entries(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<Vec<LogEntry>, SqliteError> {
        let mut stmt = self.connection.prepare_cached(sql)?;
        let mut rows = stmt.query(params)?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let text: String = row.get(0)?;
            result.push(parse_entry(&text)?);
        }
        Ok(result)
    }

    fn ensure_writable(&self) -> Result<(), SqliteError> {
        if self.read_only {
            Err(SqliteError::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Begin a write of a stored session: a write transaction and the
    /// session's row id. A session that is not stored is
    /// [`SqliteError::SessionNotFound`]. Nothing changes unless the caller
    /// commits.
    fn write(&self, session_id: &str) -> Result<(rusqlite::Transaction<'_>, i64), SqliteError> {
        validate_session_id(session_id)?;
        self.ensure_writable()?;
        let tx =
            rusqlite::Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate)?;
        let Some(session) = self.session(session_id)? else {
            return Err(SqliteError::SessionNotFound(session_id.to_owned()));
        };
        Ok((tx, session))
    }
}

/// Read-only health check of a session database: its format, SQLite's own integrity check, and foreign keys. Returns one line
/// per problem. It verifies storage, not what the entries mean, and neither
/// migrates nor repairs.
pub(crate) fn check_database(database_path: &Path) -> Result<Vec<String>, SqliteError> {
    let connection = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    let mut problems = Vec::new();
    match pending_migrations(&connection) {
        Ok(pending) if pending.is_empty() => {}
        Ok(pending) => {
            let names: Vec<_> = pending.iter().map(|(name, _)| *name).collect();
            problems.push(format!(
                "not yet migrated: {} (opening a session applies it)",
                names.join(", ")
            ));
        }
        Err(error @ (SqliteError::Version(_) | SqliteError::Format)) => {
            problems.push(error.to_string());
        }
        Err(error) => return Err(error),
    }
    let mut integrity = connection.prepare_cached("PRAGMA integrity_check")?;
    for line in integrity.query_map([], |row| row.get::<_, String>(0))? {
        let line = line?;
        if line != "ok" {
            problems.push(format!("integrity: {line}"));
        }
    }
    let mut foreign_keys = connection.prepare_cached("PRAGMA foreign_key_check")?;
    for table in foreign_keys.query_map([], |row| row.get::<_, String>(0))? {
        problems.push(format!("foreign key violation in table {}", table?));
    }
    Ok(problems)
}

/// Every task record of a session database, across its projects and
/// sessions, each with its project and session: by project and session,
/// in id order. Read-only.
pub(crate) fn task_records(
    database_path: &Path,
) -> Result<Vec<(String, String, crate::TaskRecord)>, SqliteError> {
    let connection = Connection::open_with_flags(database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    let mut statement = connection.prepare(
        "SELECT s.project, s.name, th.agent, t.id, t.call_id, t.command, t.pid,
                t.process_start, t.output_path, t.exit_path, t.started_at
         FROM tasks t
         JOIN sessions s ON s.id = t.session
         JOIN threads th ON th.id = t.thread
         ORDER BY s.project, s.name, t.id",
    )?;
    let rows = statement.query_map([], |row| {
        let agent = optional_unsigned_column(row, 2)?;
        let narrow = |wide: u64| usize::try_from(wide).unwrap_or(usize::MAX);
        let task = crate::TaskRecord {
            id: narrow(unsigned_column(row, 3)?),
            owner: agent.map_or(aj_agent::events::AgentId::Main, |agent| {
                aj_agent::events::AgentId::Sub(narrow(agent))
            }),
            call_id: row.get(4)?,
            command: row.get(5)?,
            pid: row.get(6)?,
            process_start: unsigned_column(row, 7)?,
            output_path: row.get::<_, String>(8)?.into(),
            exit_path: row.get::<_, String>(9)?.into(),
            started_at: time_column(row, 10)?,
        };
        Ok((row.get(0)?, row.get(1)?, task))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn configure(connection: &Connection) -> Result<(), SqliteError> {
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    // A commit survives a process crash. The newest ones can be lost when
    // the host loses power, which keeps the write lock short for the other
    // processes that share this database.
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    // The write-ahead log is truncated to this at a checkpoint, so one
    // large commit, an import for example, does not leave it large.
    connection.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;
    // Every statement this store runs stays compiled on its connection.
    connection.set_prepared_statement_cache_capacity(64);
    Ok(())
}

/// The migrations `connection`'s database lacks. A database is refused when
/// it holds tables without a count, or counts more migrations than this build
/// knows, which is a newer build's database.
fn pending_migrations(
    connection: &Connection,
) -> Result<&'static [(&'static str, &'static str)], SqliteError> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == 0 {
        let empty: bool = connection.query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM sqlite_master)",
            [],
            |row| row.get(0),
        )?;
        if !empty {
            return Err(SqliteError::Version(0));
        }
    }
    let applied = usize::try_from(version).map_err(|_| SqliteError::Version(version))?;
    let pending = MIGRATIONS
        .get(applied..)
        .ok_or(SqliteError::Version(version))?;
    // The count alone cannot tell this build's migrations from another
    // lineage's: a build of a branch whose migration of the same number
    // has other text leaves the count right and the tables wrong. A test
    // pins this build's own migrations. Only this check sees a database
    // that such a build wrote.
    let stamped: i64 = connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if stamped != fingerprint(&MIGRATIONS[..applied]) {
        return Err(SqliteError::Format);
    }
    Ok(pending)
}

/// What a database that applied exactly `migrations` carries in `PRAGMA
/// application_id`: a hash of their text, `0` for none.
fn fingerprint(migrations: &[(&str, &str)]) -> i64 {
    if migrations.is_empty() {
        return 0;
    }
    // FNV-1a, kept positive and nonzero so it reads back as it was set.
    let mut hash: u32 = 0x811c_9dc5;
    for byte in migrations.iter().flat_map(|(_, sql)| sql.bytes()) {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    i64::from(hash >> 1 | 1)
}

/// Apply every migration the database lacks, each in its own transaction
/// together with the count of it. Safe against a concurrent migrator: the
/// write lock orders them and the second finds nothing left to do. It waits
/// for the first no longer than any other write waits.
fn migrate(connection: &mut Connection) -> Result<(), SqliteError> {
    loop {
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = pending_migrations(&tx)?;
        let Some((_, sql)) = pending.first() else {
            return Ok(());
        };
        tx.execute_batch(sql)?;
        let applied = MIGRATIONS.len() - pending.len() + 1;
        tx.pragma_update(
            None,
            "user_version",
            i64::try_from(applied).expect("few migrations"),
        )?;
        tx.pragma_update(None, "application_id", fingerprint(&MIGRATIONS[..applied]))?;
        tx.commit()?;
    }
}

// Readers may discover the database as soon as its name exists. Build and close
// the initial WAL database privately before publishing that name, so readers
// never observe an incomplete schema or race a journal-mode switch. Closing the
// sole private connection checkpoints the schema into the main file before it
// moves, so no WAL sidecars need to move with it. A competing
// creator may win the no-replace publication; its complete database is then used.
fn install_database(directory: &Path, path: &Path) -> Result<(), SqliteError> {
    let temporary = tempfile::Builder::new()
        .prefix(".sessions-")
        .tempfile_in(directory)?;
    let mut connection = Connection::open(temporary.path())?;
    configure(&connection)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    migrate(&mut connection)?;
    connection.close().map_err(|(_, error)| error)?;
    temporary.as_file().sync_all()?;
    match temporary.persist_noclobber(path) {
        Ok(_) => {
            std::fs::File::open(directory)?.sync_all()?;
        }
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.error.into()),
    }
    Ok(())
}

// SQLite replays a write-ahead log into whatever database carries its name, so
// a log left by a database that was moved away or lost would be applied to the
// new one. A log that belongs to a database another process has just created
// is not a leftover, hence the second look for the database.
fn refuse_leftovers(path: &Path) -> Result<(), SqliteError> {
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let leftover = std::path::PathBuf::from(name);
        if leftover.try_exists()? && !path.try_exists()? {
            return Err(SqliteError::Leftover(leftover));
        }
    }
    Ok(())
}

/// The thread `owner`'s work belongs to (see [`WorkOwner`]), `None`
/// when the session has no such thread. Run inside the caller's
/// write transaction.
fn owner_thread(
    connection: &Connection,
    session: i64,
    owner: WorkOwner,
) -> Result<Option<ThreadId>, SqliteError> {
    let (thread, kind) = match owner {
        WorkOwner::Main(thread) => (thread, "main"),
        WorkOwner::Sub(thread) => (thread, "sub"),
    };
    let found = connection.query_row(
        "SELECT id FROM threads WHERE id = ?2 AND session = ?1 AND kind = ?3",
        params![session, thread, kind],
        |row| row.get(0),
    );
    Ok(found.optional()?)
}

/// Raise the session's task id counter to at least `id`, inside the
/// caller's write transaction.
fn reserve_task(connection: &Connection, session: i64, id: u64) -> Result<(), SqliteError> {
    connection.execute(
        "UPDATE sessions SET last_task = MAX(last_task, ?2) WHERE id = ?1",
        params![session, signed(id)?],
    )?;
    Ok(())
}

fn signed(value: u64) -> Result<i64, SqliteError> {
    i64::try_from(value).map_err(|_| SqliteError::Invalid("integer exceeds SQLite range".into()))
}

/// Save `row` in its owner's thread inside the caller's write
/// transaction (see [`SqliteStore::save_pending`]).
fn insert_pending(
    connection: &Connection,
    session: i64,
    owner: WorkOwner,
    row: &PendingRow,
) -> Result<(), SqliteError> {
    let delivered: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM entries WHERE session = ?1 AND entry_id = ?2)",
        params![session, row.id],
        |row| row.get(0),
    )?;
    if delivered {
        return Ok(());
    }
    let Some(thread) = owner_thread(connection, session, owner)? else {
        return Err(SqliteError::NoThread(format!("pending row {:?}", row.id)));
    };
    connection.execute(
        "INSERT INTO pending (session, id, thread, kind, body)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (session, id) DO UPDATE SET kind = excluded.kind, body = excluded.body",
        params![session, row.id, thread, row.kind, row.body],
    )?;
    Ok(())
}

fn validate_project_id(id: &str) -> Result<(), SqliteError> {
    if id.is_empty() {
        Err(SqliteError::Invalid("empty project identity".into()))
    } else {
        Ok(())
    }
}

pub(crate) fn validate_tag(tag: Option<&str>) -> Result<(), SqliteError> {
    if let Some(tag) = tag
        && crate::tag::normalize_tag(tag).ok().flatten().as_deref() != Some(tag)
    {
        return Err(SqliteError::Invalid(
            "tag is not a normalized session label".into(),
        ));
    }
    Ok(())
}

fn validate_session_id(id: &str) -> Result<(), SqliteError> {
    if crate::id::is_valid_session_id(id) {
        Ok(())
    } else {
        Err(SqliteError::InvalidSessionId(id.to_owned()))
    }
}

fn integer(value: u64) -> Result<i64, SqliteError> {
    i64::try_from(value).map_err(|_| SqliteError::Invalid("integer exceeds SQLite range".into()))
}

fn limit_integer(value: usize) -> Result<i64, SqliteError> {
    i64::try_from(value).map_err(|_| SqliteError::Invalid("integer exceeds SQLite range".into()))
}

fn unsigned_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn optional_unsigned_column(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<u64>> {
    let value: Option<i64> = row.get(index)?;
    value
        .map(|value| {
            u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
        })
        .transpose()
}

/// A position as a query bound. One past what SQLite can count bounds
/// nothing.
fn bound(seq: u64) -> i64 {
    i64::try_from(seq).unwrap_or(i64::MAX)
}

/// A fixed-width UTC timestamp, so stored times compare as text.
fn timestamp(time: DateTime<Utc>) -> String {
    time.to_rfc3339_opts(SecondsFormat::Micros, true)
}

fn parse_entry(text: &str) -> Result<LogEntry, SqliteError> {
    let mut entry: LogEntry = serde_json::from_str(text)?;
    if let LogEntryKind::Message { message } = &mut entry.entry {
        message.set_id(entry.id.clone());
    }
    Ok(entry)
}

/// The row type of an entry: what kind of message it is, or the entry's own
/// type for a state entry.
///
/// A state entry of a type [`threads::STATE_TYPES`] does not name is
/// refused. A reader finds the state entries before its window by that
/// list, so a row outside it would be stored and then not read.
fn row_type<'a>(entry: &LogEntry, text: &'a str) -> Result<&'a str, SqliteError> {
    #[derive(serde::Deserialize)]
    struct Head<'a> {
        #[serde(rename = "type")]
        kind: &'a str,
    }
    if let Some(message) = entry.entry.message_type() {
        return Ok(message);
    }
    let kind = serde_json::from_str::<Head<'a>>(text)?.kind;
    if !threads::STATE_TYPES.contains(&kind) {
        return Err(SqliteError::Invalid(format!(
            "entry {:?} has type {kind:?}, which the store does not file",
            entry.id
        )));
    }
    Ok(kind)
}

fn time_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<DateTime<Utc>> {
    let text: String = row.get(index)?;
    let time = DateTime::parse_from_rfc3339(&text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    Ok(time.with_timezone(&Utc))
}

fn metadata_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, SessionMetadata)> {
    let updated_at = time_column(row, 3)?;
    Ok((
        row.get(0)?,
        SessionMetadata {
            id: row.get(1)?,
            last_seq: unsigned_column(row, 2)?,
            updated_at,
        },
    ))
}

/// The row id of a stored session.
fn session_row(
    connection: &Connection,
    project_id: &str,
    session_id: &str,
) -> Result<Option<i64>, SqliteError> {
    Ok(connection
        .query_row(
            "SELECT id FROM sessions WHERE project = ?1 AND name = ?2",
            params![project_id, session_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// A stored session with its row id.
fn stored_session(
    connection: &Connection,
    project_id: &str,
    session_id: &str,
) -> Result<Option<(i64, SessionMetadata)>, SqliteError> {
    Ok(connection
        .query_row(
            "SELECT id, name, last_seq, updated_at FROM sessions
             WHERE project = ?1 AND name = ?2",
            params![project_id, session_id],
            metadata_row,
        )
        .optional()?)
}

/// Each entry of a batch as it is stored. Done before the write
/// transaction, so the database is not held while a large batch, an import
/// for example, is serialized.
fn serialize(entries: &[&LogEntry]) -> Result<Vec<String>, SqliteError> {
    let entries = entries.iter().map(serde_json::to_string);
    Ok(entries.collect::<Result<_, _>>()?)
}

/// Where the entry an entry continues from sits.
struct Parent {
    seq: u64,
    thread: ThreadId,
    owner: Owner,
    /// The position of its thread's last entry.
    thread_end: u64,
}

/// Decide where `entry` goes, inside the caller's write transaction. This
/// is the one rule that files a stored entry, for an append and an import
/// alike. Returns the thread, `None` for one the entry starts, and
/// how the entry lands there.
///
/// `first` says the session holds no entry yet. Only then may an entry
/// name no parent, and it starts the session's first thread. Every
/// other entry names an entry of the session.
///
/// `on` is the main thread the writer's main agent is on. While that
/// thread holds no entry, an entry of the main agent that continues
/// from where it starts is its first entry. The writer names it because
/// nothing else tells an empty thread from a sibling that starts at
/// the same entry.
///
/// A meta entry is filed beside the entries of the first thread.
/// Any other entry continues its parent's thread when that has the
/// entry's owner and the parent is its last entry. Otherwise it starts
/// a thread that continues from the parent: a branch or a sub-agent's
/// run.
///
/// The system prompt stays the first thread's last entry until a
/// entry follows it, whatever meta entries are filed beside it. So the
/// first entry after the prompt continues the first thread, and a
/// later one that starts over at the prompt is a branch.
fn place(
    connection: &Connection,
    session: i64,
    entry: &LogEntry,
    first: bool,
    on: Option<ThreadId>,
) -> Result<(Option<ThreadId>, Landing), SqliteError> {
    let refuse = |reason: &str| SqliteError::Invalid(format!("entry {:?} {reason}", entry.id));
    if (entry.thread == ThreadKind::Sub) != entry.agent_id.is_some() {
        return Err(refuse("has inconsistent thread and agent"));
    }
    let owner = Owner::of(entry.thread, entry.agent_id);
    let parent = match &entry.parent_id {
        None if first => None,
        None => return Err(refuse("must be the first root or have an earlier parent")),
        Some(parent) => {
            let mut find = connection.prepare_cached(
                "SELECT e.seq, th.id, th.agent, th.last_seq
                 FROM entries e JOIN threads th ON th.id = e.thread
                 WHERE e.session = ?1 AND e.entry_id = ?2",
            )?;
            let found = find.query_row(params![session, parent], |row| {
                let agent: Option<i64> = row.get(2)?;
                let owner = match agent {
                    Some(agent) => Owner::Sub(usize::try_from(agent).unwrap_or(usize::MAX)),
                    None => Owner::Main,
                };
                Ok(Parent {
                    seq: unsigned_column(row, 0)?,
                    thread: row.get(1)?,
                    owner,
                    thread_end: unsigned_column(row, 3)?,
                })
            });
            let found = found.optional()?;
            Some(found.ok_or_else(|| refuse("must be the first root or have an earlier parent"))?)
        }
    };
    if entry.thread == ThreadKind::Meta && !first {
        let mut root =
            connection.prepare_cached("SELECT MIN(id) FROM threads WHERE session = ?1")?;
        let root: Option<ThreadId> = root.query_row([session], |row| row.get(0))?;
        let root = root.ok_or_else(|| refuse("is in a session without a thread"))?;
        return Ok((Some(root), Landing::Beside));
    }
    if let (Some(on), Owner::Main) = (on, owner) {
        let from = parent.as_ref().map(|parent| (parent.thread, parent.seq));
        let mut empty = connection.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM threads
             WHERE id = ?2 AND session = ?1 AND kind = 'main'
               AND parent IS ?3 AND forked_at IS ?4 AND last_seq = COALESCE(forked_at, 0))",
        )?;
        let at = from.map(|(_, at)| integer(at)).transpose()?;
        let thread = from.map(|(thread, _)| thread);
        if empty.query_row(params![session, on, thread, at], |row| row.get(0))? {
            return Ok((Some(on), Landing::Continues));
        }
    }
    Ok(match parent {
        Some(parent) if parent.owner == owner && parent.thread_end == parent.seq => {
            (Some(parent.thread), Landing::Continues)
        }
        parent => {
            let from = parent.map(|parent| (parent.thread, parent.seq));
            (None, Landing::Starts { from })
        }
    })
}

/// File `entries`, whose stored forms are `stored`, after the session's
/// last entry and move the session's counters, inside the caller's write
/// transaction: a failure rolls back all of it. `metadata` is the session
/// as the transaction found it.
///
/// `live` says a running host appends the entries, as opposed to an
/// import. Then each entry retires the pending row it delivers, and a
/// sub-agent's run starts as working. `on` is the thread
/// [`SqliteStore::append`] describes.
#[allow(clippy::too_many_arguments)]
fn insert_batch(
    connection: &Connection,
    session: i64,
    mut metadata: SessionMetadata,
    entries: &[&LogEntry],
    stored: &[String],
    updated_at: DateTime<Utc>,
    live: bool,
    on: Option<ThreadId>,
) -> Result<Appended, SqliteError> {
    let mut landed = Vec::with_capacity(entries.len());
    let mut head_thread = None;
    // Compiled once for the batch, which an import makes tens of thousands
    // of entries long.
    let mut start_thread = connection.prepare_cached(
        "INSERT INTO threads (session, kind, agent, parent, forked_at, last_seq, turn)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    // The next entry is filed by where this one's thread ends, so
    // the end moves with each entry.
    let mut continue_thread =
        connection.prepare_cached("UPDATE threads SET last_seq = ?2 WHERE id = ?1")?;
    let mut retire_pending =
        connection.prepare_cached("DELETE FROM pending WHERE session = ?1 AND id = ?2")?;
    let mut insert_entry = connection.prepare_cached(
        "INSERT INTO entries (session, seq, thread, entry_id, type, entry)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for (entry, text) in entries.iter().zip(stored) {
        if entry.id.is_empty() {
            return Err(SqliteError::Invalid("empty entry identity".into()));
        }
        let seq = metadata
            .last_seq
            .checked_add(1)
            .ok_or_else(|| SqliteError::Invalid("position exceeds SQLite range".into()))?;
        let at = integer(seq)?;
        let first = metadata.last_seq == 0;
        let (thread, landing) = place(connection, session, entry, first, on)?;
        let thread = match (thread, landing) {
            (Some(thread), Landing::Continues) => {
                continue_thread.execute(params![thread, at])?;
                thread
            }
            (Some(thread), _) => thread,
            (None, _) => {
                let owner = Owner::of(entry.thread, entry.agent_id);
                let (kind, agent) = match owner {
                    Owner::Main => ("main", None),
                    Owner::Sub(agent) => ("sub", Some(limit_integer(agent)?)),
                };
                let from = match landing {
                    Landing::Starts {
                        from: Some((thread, forked_at)),
                    } => Some((thread, integer(forked_at)?)),
                    _ => None,
                };
                let spawned = matches!(entry.entry, LogEntryKind::SubAgentSpawn { .. });
                let turn = if live && spawned { "working" } else { "idle" };
                start_thread.execute(params![
                    session,
                    kind,
                    agent,
                    from.map(|(thread, _)| thread),
                    from.map(|(_, forked_at)| forked_at),
                    at,
                    turn
                ])?;
                connection.last_insert_rowid()
            }
        };
        // A sub-agent's entry is in its run's thread and a meta
        // entry is beside the first one's entries. Every other entry is
        // an entry of a main thread.
        if entry.thread != ThreadKind::Sub && landing != Landing::Beside {
            head_thread = Some(thread);
        }
        // Delivering a pending row is appending the entry that carries its id.
        if live {
            retire_pending.execute(params![session, entry.id])?;
        }
        let inserted = insert_entry.execute(params![
            session,
            at,
            thread,
            entry.id,
            row_type(entry, text)?,
            text
        ]);
        match inserted {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                return Err(SqliteError::EntryExists {
                    session_id: metadata.id.clone(),
                    entry_id: entry.id.clone(),
                });
            }
            Err(error) => return Err(error.into()),
        }
        metadata.last_seq = seq;
        landed.push(Landed {
            seq,
            thread,
            landing,
        });
    }
    if !entries.is_empty() {
        metadata.updated_at = updated_at;
        connection.execute(
            "UPDATE sessions SET last_seq = ?2, updated_at = ?3 WHERE id = ?1",
            params![session, integer(metadata.last_seq)?, timestamp(updated_at)],
        )?;
    }
    Ok(Appended {
        metadata,
        landed,
        head_thread,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aj_agent::message::AgentMessage;

    /// A commit survives a process crash without a sync per commit.
    #[test]
    fn a_writer_syncs_at_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteStore::open_project(&dir.path().join("s.sqlite3"), "p").unwrap();
        let pragma = |name| -> String {
            let sql = format!("PRAGMA {name}");
            let value: rusqlite::types::Value = store
                .connection
                .query_row(&sql, [], |row| row.get(0))
                .unwrap();
            format!("{value:?}")
        };
        assert_eq!(pragma("journal_mode"), "Text(\"wal\")");
        assert_eq!(pragma("synchronous"), "Integer(1)");
    }

    fn time() -> DateTime<Utc> {
        "2026-11-09T12:34:56Z".parse().unwrap()
    }

    fn plain(id: &str, parent: Option<&str>) -> LogEntry {
        LogEntry {
            id: id.into(),
            parent_id: parent.map(str::to_owned),
            timestamp: Some(time()),
            thread: ThreadKind::Main,
            agent_id: None,
            entry: LogEntryKind::Message {
                message: AgentMessage::internal_context(format!("record {id}")),
            },
        }
    }

    /// Create session `s` with `first` as its only entry. Returns the
    /// thread it started.
    fn create(store: &mut SqliteStore, first: &LogEntry) -> ThreadId {
        let none = SessionAnnotations::default();
        let created = store.create_session("s", &[first], time(), &none).unwrap();
        created.landed[0].thread
    }

    fn ids(store: &SqliteStore, session: &str) -> Vec<String> {
        let entries = store.full_entries(session).unwrap();
        entries.into_iter().map(|e| e.id).collect()
    }

    fn open(dir: &Path, project: &str) -> SqliteStore {
        SqliteStore::open_project(&dir.join("sessions.sqlite3"), project).unwrap()
    }

    #[test]
    fn projects_sharing_a_database_keep_identical_session_ids_apart() {
        let dir = tempfile::tempdir().unwrap();
        let mut a = open(dir.path(), "a");
        let mut b = open(dir.path(), "b");
        create(&mut a, &plain("root", None));
        create(&mut b, &plain("other", None));
        let tail = plain("tail", Some("other"));
        b.append("s", &[&tail], time(), None).unwrap();
        b.set_tag("s", Some("label")).unwrap();

        assert_eq!(ids(&a, "s"), ["root"]);
        assert_eq!(ids(&b, "s"), ["other", "tail"]);
        assert_eq!(a.list_sessions().unwrap().len(), 1);
        assert_eq!(a.annotations("s").unwrap(), SessionAnnotations::default());
        assert_eq!(a.project_ids().unwrap(), ["a", "b"]);
    }

    #[test]
    fn a_refused_or_failed_append_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "p");
        create(&mut store, &plain("root", None));
        let before = store.session_metadata("s").unwrap();
        let later = time() + chrono::Duration::seconds(1);
        let good = plain("good", Some("root"));
        let again = plain("root", Some("good"));
        // Another session's entry is not one of this session's.
        let none = SessionAnnotations::default();
        store
            .create_session("other", &[&plain("theirs", None)], time(), &none)
            .unwrap();
        let foreign = plain("foreign", Some("theirs"));
        let second_root = plain("second", None);
        let agent_on_main = owned("mixed", Some("root"), ThreadKind::Main, Some(1));
        let sub_without_agent = owned("mixed", Some("root"), ThreadKind::Sub, None);

        // A reused identity, a parent the session does not have, a second
        // root, and an entry whose agent contradicts its thread. A good
        // entry before the refused one is rolled back with it.
        let batches: [(&[&LogEntry], fn(&SqliteError) -> bool); 5] = [
            (&[&good, &again], |e| {
                matches!(e, SqliteError::EntryExists { .. })
            }),
            (&[&foreign], |e| matches!(e, SqliteError::Invalid(_))),
            (&[&good, &second_root], |e| {
                matches!(e, SqliteError::Invalid(_))
            }),
            (&[&agent_on_main], |e| matches!(e, SqliteError::Invalid(_))),
            (&[&sub_without_agent], |e| {
                matches!(e, SqliteError::Invalid(_))
            }),
        ];
        for (batch, expected) in batches {
            let refused = store.append("s", batch, later, None).unwrap_err();
            assert!(expected(&refused), "{refused}");
            assert_eq!(store.session_metadata("s").unwrap(), before);
            assert_eq!(store.threads("s").unwrap().len(), 1);
            assert_eq!(ids(&store, "s"), ["root"]);
        }
        // A session that is not stored takes no append.
        assert!(matches!(
            store.append("absent", &[&good], later, None),
            Err(SqliteError::SessionNotFound(_))
        ));

        // A storage failure on the last statement rolls the inserted
        // entries and the thread they started back.
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail BEFORE UPDATE OF last_seq ON sessions
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        let sub = owned("sub", Some("root"), ThreadKind::Sub, Some(1));
        assert!(store.append("s", &[&good, &sub], later, None).is_err());
        assert_eq!(store.session_metadata("s").unwrap(), before);
        assert_eq!(store.threads("s").unwrap().len(), 1);
        assert_eq!(ids(&store, "s"), ["root"]);
    }

    /// A write that finds the database locked for longer than the busy
    /// timeout is busy, and has changed nothing.
    #[test]
    fn a_write_another_connection_locks_out_is_busy_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "p");
        let main = create(&mut store, &plain("root", None));
        store.set_busy_timeout(Duration::from_millis(5)).unwrap();
        let other = Connection::open(dir.path().join("sessions.sqlite3")).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();

        let next = plain("next", Some("root"));
        let pending = PendingRow {
            id: "q".into(),
            kind: "follow_up".into(),
            body: "later".into(),
        };
        let owner = WorkOwner::Main(main);
        let fork = Creation {
            session: None,
            from: Some("root"),
            entries: &[],
            input: Some(&pending),
            turn: crate::TurnState::Idle,
        };
        let refusals = [
            store.append("s", &[&next], time(), None).err(),
            store.save_pending("s", owner, &pending).err(),
            store.delete_pending("s", "q").err(),
            store.reserve_task("s", 3).err(),
            store.set_turn("s", main, crate::TurnState::Working).err(),
            store.create_thread("s", &fork, time()).err(),
        ];
        for refused in refusals {
            let refused = refused.expect("a locked database takes no write");
            assert!(refused.is_busy(), "{refused}");
        }
        other.execute_batch("COMMIT").unwrap();

        assert_eq!(ids(&store, "s"), ["root"]);
        assert_eq!(store.turn("s", main).unwrap(), crate::TurnState::Idle);
        assert_eq!(store.last_task("s").unwrap(), 0);
        // The same writes land once the lock is gone.
        store.append("s", &[&next], time(), None).unwrap();
        assert_eq!(ids(&store, "s"), ["root", "next"]);
    }

    #[test]
    fn reopening_reads_committed_entries_metadata_and_pages() {
        let dir = tempfile::tempdir().unwrap();
        let written = {
            let mut store = open(dir.path(), "p");
            create(&mut store, &plain("a", None));
            let (b, c) = (plain("b", Some("a")), plain("c", Some("b")));
            store.append("s", &[&b, &c], time(), None).unwrap().metadata
        };
        let store =
            SqliteStore::open_project_read_only(&dir.path().join("sessions.sqlite3"), "p").unwrap();
        assert_eq!(store.session_metadata("s").unwrap(), Some(written.clone()));
        assert_eq!(written.last_seq, 3);
        assert_eq!(ids(&store, "s"), ["a", "b", "c"]);
        let page = store.entries_at("s", &[2]).unwrap();
        assert_eq!((page.len(), page[0].id.as_str()), (1, "b"));
        // The size is the entries' own, whatever the database allocates.
        let bytes: usize = store
            .full_entries("s")
            .unwrap()
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap().len())
            .sum();
        assert_eq!(
            store.entry_bytes("s").unwrap(),
            u64::try_from(bytes).unwrap()
        );
    }

    #[test]
    fn annotations_belong_to_a_stored_session_and_read_only_handles_refuse_writes() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "p");
        // A session that is not stored has no row to carry a label.
        assert!(matches!(
            store.set_tag("s", Some("label")),
            Err(SqliteError::SessionNotFound(_))
        ));
        assert!(matches!(
            store.set_archived("s", true),
            Err(SqliteError::SessionNotFound(_))
        ));
        assert!(!store.contains_session("s").unwrap());
        assert_eq!(
            store.annotations("s").unwrap(),
            SessionAnnotations::default()
        );

        // The session is created with the ones its writer held for it.
        let expected = SessionAnnotations {
            tag: Some("label".into()),
            archived: true,
        };
        let root = plain("root", None);
        let unnormalized = SessionAnnotations {
            tag: Some(" not normalized ".into()),
            archived: false,
        };
        let batch = [&root];
        assert!(
            store
                .create_session("s", &batch, time(), &unnormalized)
                .is_err()
        );
        assert!(!store.contains_session("s").unwrap());
        store
            .create_session("s", &batch, time(), &expected)
            .unwrap();
        assert_eq!(store.annotations("s").unwrap(), expected);
        assert!(store.set_tag("s", Some(" not normalized ")).is_err());

        let reader =
            SqliteStore::open_project_read_only(&dir.path().join("sessions.sqlite3"), "p").unwrap();
        assert!(matches!(
            reader.set_archived("s", false),
            Err(SqliteError::ReadOnly)
        ));
        assert_eq!(reader.annotations("s").unwrap(), expected);
    }

    fn owned(
        id: &str,
        parent: Option<&str>,
        thread: ThreadKind,
        agent_id: Option<usize>,
    ) -> LogEntry {
        LogEntry {
            thread,
            agent_id,
            ..plain(id, parent)
        }
    }

    fn threads(store: &SqliteStore, session: &str) -> Vec<Row> {
        let rows = store.threads(session).unwrap();
        rows.into_iter()
            .map(|row| {
                (
                    row.id,
                    row.owner,
                    row.parent,
                    row.last_seq,
                    row.last_entry_id,
                )
            })
            .collect()
    }

    type Row = (ThreadId, Owner, Option<(ThreadId, u64)>, u64, String);

    /// The thread each entry of the session is filed under, in
    /// append order.
    fn filed_under(store: &SqliteStore, session: &str) -> Vec<ThreadId> {
        let (_, records) = store.whole(session).unwrap();
        let filed = records.into_iter();
        filed.map(|(thread, _)| thread).collect()
    }

    /// The store files each entry by the entry it continues from and
    /// the agent it is for, answers where it landed, and numbers the
    /// threads in the order they start.
    #[test]
    fn the_store_files_each_entry_and_reads_threads_back_in_append_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "p");
        let entries = [
            owned("root", None, ThreadKind::Meta, None),
            owned("u1", Some("root"), ThreadKind::Main, None),
            owned("s1", Some("u1"), ThreadKind::Sub, Some(1)),
            owned("u2", Some("u1"), ThreadKind::Main, None),
            owned("b1", Some("u1"), ThreadKind::Main, None),
            owned("context", Some("root"), ThreadKind::Meta, None),
            owned("r1", Some("root"), ThreadKind::Main, None),
        ];
        // The session starts with one thread, which the first
        // entry after the root continues.
        let none = SessionAnnotations::default();
        let created = store
            .create_session("s", &[&entries[0], &entries[1]], time(), &none)
            .unwrap();
        let main = created.landed[0].thread;
        let at = |seq, thread, landing| Landed {
            seq,
            thread,
            landing,
        };
        assert_eq!(
            created.landed,
            [
                at(1, main, Landing::Starts { from: None }),
                at(2, main, Landing::Continues)
            ]
        );
        // A sub-agent run and a branch both continue from `u1`, and the
        // branch is told apart from the thread it leaves. An entry
        // filed beside a thread does not move its end. A branch may
        // continue from the first entry.
        let batch: Vec<&LogEntry> = entries[2..].iter().collect();
        let appended = store.append("s", &batch, time(), None).unwrap();
        let started = |index: usize| appended.landed[index].thread;
        let (sub, branch, restart) = (started(0), started(2), started(4));
        assert!(main < sub && sub < branch && branch < restart);
        let from = |seq| Landing::Starts {
            from: Some((main, seq)),
        };
        assert_eq!(
            appended.landed,
            [
                at(3, sub, from(2)),
                at(4, main, Landing::Continues),
                at(5, branch, from(2)),
                at(6, main, Landing::Beside),
                at(7, restart, from(1)),
            ]
        );
        assert_eq!(appended.head_thread, Some(restart));
        // A run a host spawns starts as working, and this one's first
        // entry is no spawn.
        assert_eq!(store.turn("s", sub).unwrap(), crate::TurnState::Idle);

        let expected = vec![
            (main, Owner::Main, None, 4, "u2".to_owned()),
            (sub, Owner::Sub(1), Some((main, 2)), 3, "s1".to_owned()),
            (branch, Owner::Main, Some((main, 2)), 5, "b1".to_owned()),
            (restart, Owner::Main, Some((main, 1)), 7, "r1".to_owned()),
        ];
        assert_eq!(threads(&store, "s"), expected);
        assert_eq!(
            filed_under(&store, "s"),
            [main, main, sub, main, branch, main, restart]
        );
        let written: Vec<_> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids(&store, "s"), written);

        drop(store);
        let reopened =
            SqliteStore::open_project_read_only(&dir.path().join("sessions.sqlite3"), "p").unwrap();
        assert_eq!(threads(&reopened, "s"), expected);
        assert_eq!(
            check_database(&dir.path().join("sessions.sqlite3")).unwrap(),
            [""; 0]
        );
    }

    /// A small deterministic generator, so a failing case can be run again.
    struct Dice(u64);

    impl Dice {
        fn roll(&mut self, sides: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            usize::try_from(self.0 % u64::try_from(sides).unwrap()).unwrap()
        }
    }

    /// A session's entries in append order, each naming an earlier one as
    /// its parent: main entries that continue the last one or an earlier
    /// one, sub-agent runs that start on any entry and are continued,
    /// forked or taken over by another sub-agent, and meta entries.
    fn random_session(dice: &mut Dice, len: usize) -> Vec<LogEntry> {
        let mut entries = vec![owned("1", None, ThreadKind::Meta, None)];
        // The positions of the main entries and of each sub-agent's.
        let mut main: Vec<usize> = vec![1];
        let mut subs: Vec<Vec<usize>> = Vec::new();
        while entries.len() < len {
            let seq = entries.len() + 1;
            let any = |dice: &mut Dice, of: &[usize]| of[dice.roll(of.len())];
            let (thread, agent, parent) = match dice.roll(10) {
                // The main agent goes on from its last entry.
                0..=3 => (ThreadKind::Main, None, *main.last().unwrap()),
                // It starts over at an earlier one.
                4 => (ThreadKind::Main, None, any(dice, &main)),
                // A sub-agent is spawned, under a new number or a used one.
                5 => {
                    let n = dice.roll(subs.len() + 1);
                    (ThreadKind::Sub, Some(n), any(dice, &main))
                }
                // A sub-agent goes on from its last entry, or from an
                // earlier one, or from another sub-agent's.
                6..=8 if !subs.is_empty() => {
                    let n = dice.roll(subs.len());
                    let from = match dice.roll(6) {
                        0 => any(dice, &subs[n]),
                        1 => {
                            let other = dice.roll(subs.len());
                            any(dice, &subs[other])
                        }
                        _ => *subs[n].last().unwrap(),
                    };
                    (ThreadKind::Sub, Some(n), from)
                }
                // A meta entry, which may name any entry as its parent.
                _ => (ThreadKind::Meta, None, dice.roll(seq - 1) + 1),
            };
            match (thread, agent) {
                (ThreadKind::Main, _) => main.push(seq),
                (ThreadKind::Sub, Some(n)) if n == subs.len() => subs.push(vec![seq]),
                (ThreadKind::Sub, Some(n)) => subs[n].push(seq),
                _ => {}
            }
            let parent = parent.to_string();
            entries.push(owned(&seq.to_string(), Some(&parent), thread, agent));
        }
        entries
    }

    /// What the rule files `entries` as when it is applied in memory, one
    /// entry at a time (see [`crate::threads::Threads::place`]): each
    /// thread in the order it starts, as its owner, the position it
    /// continues from with that position's thread, and its last
    /// position. Then the thread of each entry.
    type Filing = (Vec<(Owner, Option<(usize, u64)>, u64)>, Vec<usize>);

    fn filed_in_memory(entries: &[LogEntry]) -> Filing {
        let mut threads = crate::threads::Threads::default();
        let mut of: Vec<usize> = Vec::new();
        for (seq, entry) in (1..).zip(entries) {
            let parent = entry.parent_id.as_ref().map(|parent| {
                let at: u64 = parent.parse().unwrap();
                (of[usize::try_from(at).unwrap() - 1], at)
            });
            let filing = threads.place(entry.thread, entry.agent_id, parent);
            threads.file(&filing, seq, &entry.id);
            of.push(filing.thread);
        }
        let threads = threads.iter().map(|t| (t.owner, t.parent, t.tip));
        (threads.collect(), of)
    }

    /// What the store holds of `session`, in the terms of
    /// [`filed_in_memory`]: a thread is named by its place in the
    /// order they started.
    fn filed_in_store(store: &SqliteStore, session: &str) -> Filing {
        let rows = threads(store, session);
        let place = |id: ThreadId| rows.iter().position(|row| row.0 == id).unwrap();
        let threads = rows.iter().map(|(_, owner, parent, last, _)| {
            let parent = parent.map(|(id, at)| (place(id), at));
            (*owner, parent, *last)
        });
        let of = filed_under(store, session).into_iter().map(place);
        (threads.collect(), of.collect())
    }

    /// The placement rule as a property. Whatever entries a session is
    /// given, in whatever batches, the store files them as the rule says,
    /// its answers say where it filed them, and the session's export files
    /// the same when it is imported.
    #[test]
    fn the_store_files_any_session_by_the_rule_and_an_import_files_it_the_same() {
        let none = SessionAnnotations::default();
        // How often the sessions took each turn the rule distinguishes: a
        // branch, a sub-agent's run on a main entry, a run that leaves
        // another run, a continued run, and an entry filed beside.
        let mut seen = [0usize; 5];
        for seed in 1..=150u64 {
            let mut dice = Dice(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let len = 2 + dice.roll(70);
            let entries = random_session(&mut dice, len);
            let expected = filed_in_memory(&entries);

            // Appended by a host, in batches of random length.
            let mut store = SqliteStore::in_memory("p").unwrap();
            let mut rest: &[LogEntry] = &entries;
            let mut landed = Vec::new();
            while !rest.is_empty() {
                let (batch, later) = rest.split_at(1 + dice.roll(rest.len().min(4)));
                let batch: Vec<&LogEntry> = batch.iter().collect();
                let appended = if landed.is_empty() {
                    store.create_session("s", &batch, time(), &none)
                } else {
                    store.append("s", &batch, time(), None)
                };
                landed.extend(appended.unwrap().landed);
                rest = later;
            }
            assert_eq!(filed_in_store(&store, "s"), expected, "seed {seed}");

            // The answers name the same places.
            let rows = threads(&store, "s");
            let place = |id: ThreadId| rows.iter().position(|row| row.0 == id).unwrap();
            let mut started = 0;
            for ((seq, answer), entry) in (1..).zip(&landed).zip(&entries) {
                let at = usize::try_from(seq).unwrap() - 1;
                assert_eq!(answer.seq, seq, "seed {seed}");
                assert_eq!(place(answer.thread), expected.1[at], "seed {seed}");
                let (_, parent, last) = expected.0[expected.1[at]];
                match answer.landing {
                    Landing::Starts { from } => {
                        assert_eq!(place(answer.thread), started, "seed {seed}");
                        let from = from.map(|(id, at)| (place(id), at));
                        assert_eq!(from, parent, "seed {seed}");
                        started += 1;
                        let left = from.map(|(thread, _)| expected.0[thread].0);
                        match (entry.thread, left) {
                            (ThreadKind::Main, Some(_)) => seen[0] += 1,
                            (ThreadKind::Sub, Some(Owner::Main)) => seen[1] += 1,
                            (ThreadKind::Sub, Some(Owner::Sub(_))) => seen[2] += 1,
                            _ => {}
                        }
                    }
                    Landing::Beside => {
                        assert_eq!(entry.thread, ThreadKind::Meta);
                        seen[4] += 1;
                    }
                    Landing::Continues => {
                        assert!(seq <= last, "seed {seed}");
                        seen[3] += usize::from(entry.thread == ThreadKind::Sub);
                    }
                }
            }
            assert_eq!(started, expected.0.len(), "seed {seed}");

            // Exported as its entries in append order, and imported whole.
            let exported: Vec<String> = store
                .full_entries("s")
                .unwrap()
                .iter()
                .map(|entry| serde_json::to_string(entry).unwrap())
                .collect();
            let imported: Vec<LogEntry> = exported
                .iter()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let imported: Vec<&LogEntry> = imported.iter().collect();
            store
                .create_session("copy", &imported, time(), &none)
                .unwrap();
            assert_eq!(filed_in_store(&store, "copy"), expected, "seed {seed}");
        }
        assert!(seen.iter().all(|times| *times > 100), "{seen:?}");
    }

    /// Rows reach a session only through foreign keys, which SQLite
    /// enforces per connection.
    #[test]
    fn deleting_a_session_row_deletes_everything_filed_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "p");
        let main = create(&mut store, &plain("root", None));
        let pending = PendingRow {
            id: "q".into(),
            kind: "follow_up".into(),
            body: "later".into(),
        };
        store
            .save_pending("s", WorkOwner::Main(main), &pending)
            .unwrap();
        let task = TaskRow {
            id: 1,
            call_id: "call".into(),
            command: "sleep 1".into(),
            pid: 4242,
            process_start: 1,
            output_path: "out".into(),
            exit_path: "exit".into(),
            started_at: time(),
        };
        store
            .insert_task("s", WorkOwner::Main(main), &task)
            .unwrap();
        // A sub-agent's entry starts a second thread.
        let run = owned("run", Some("root"), ThreadKind::Sub, Some(1));
        store.append("s", &[&run], time(), None).unwrap();
        let rows = |store: &SqliteStore| -> Vec<i64> {
            ["threads", "entries", "pending", "tasks"]
                .iter()
                .map(|table| {
                    let sql = format!("SELECT COUNT(*) FROM {table}");
                    store.connection.query_row(&sql, [], |r| r.get(0)).unwrap()
                })
                .collect()
        };
        assert_eq!(rows(&store), [2, 2, 1, 1]);

        store
            .connection
            .execute("DELETE FROM sessions", [])
            .unwrap();
        assert_eq!(rows(&store), [0, 0, 0, 0]);
    }

    #[test]
    fn a_turn_is_set_on_a_thread_of_its_session_or_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = open(dir.path(), "p");
        let main = create(&mut store, &plain("root", None));
        let none = SessionAnnotations::default();
        let root = plain("root", None);
        let other = store
            .create_session("other", &[&root], time(), &none)
            .unwrap()
            .landed[0]
            .thread;

        for missing in [other, other + 1] {
            let refused = store.set_turn("s", missing, crate::TurnState::Working);
            assert!(matches!(refused, Err(SqliteError::Invalid(_))));
        }
        assert_eq!(store.turn("s", main).unwrap(), crate::TurnState::Idle);
        assert_eq!(store.turn("other", other).unwrap(), crate::TurnState::Idle);

        store
            .set_turn("s", main, crate::TurnState::Working)
            .unwrap();
        assert_eq!(store.turn("s", main).unwrap(), crate::TurnState::Working);
    }

    /// One entry of `kind`.
    fn entry(
        id: &str,
        parent: Option<&str>,
        thread: ThreadKind,
        agent_id: Option<usize>,
        kind: LogEntryKind,
    ) -> LogEntry {
        LogEntry {
            entry: kind,
            ..owned(id, parent, thread, agent_id)
        }
    }

    fn said(input: u64, stop: aj_models::types::StopReason) -> LogEntryKind {
        use aj_models::types::{AssistantMessage, Message, Usage};
        let usage = Usage {
            input,
            total_tokens: input,
            ..Usage::default()
        };
        let message = Message::Assistant(AssistantMessage {
            usage,
            stop_reason: stop,
            ..AssistantMessage::empty()
        });
        LogEntryKind::Message {
            message: AgentMessage::wire(message),
        }
    }

    fn asked(text: &str) -> LogEntryKind {
        use aj_models::types::{Message, UserMessage};
        LogEntryKind::Message {
            message: AgentMessage::wire(Message::User(UserMessage::text(text))),
        }
    }

    /// Store session `s` with what a real one holds: a system prompt, two
    /// turns with a settings change, a response that was cut off, a
    /// sub-agent's run, a branch with a compaction, a label, pending work and
    /// a task for the main agent and for the sub-agent.
    fn a_session_with_everything(store: &mut SqliteStore) {
        use aj_models::types::StopReason;
        use {ThreadKind::Main, ThreadKind::Meta, ThreadKind::Sub};
        let settings = aj_agent::events::AgentSettings {
            context_window: 0,
            provider: "p".into(),
            model_id: "m".into(),
            thinking: "off".into(),
            thinking_display: String::new(),
            speed: "default".into(),
            verbosity: "default".into(),
        };
        let prompt = LogEntryKind::SystemPrompt {
            text: "be brief".into(),
        };
        let model = LogEntryKind::ModelChange {
            provider: "p".into(),
            model_id: "m2".into(),
            context_window: 0,
        };
        let spawn = LogEntryKind::SubAgentSpawn {
            task: "look".into(),
            tool_name: "agent".into(),
            call_id: None,
            background: false,
            settings,
        };
        let end = LogEntryKind::SubAgentEnd {
            conclusion: Default::default(),
            report: "found".into(),
        };
        let compaction = LogEntryKind::Compaction {
            summary: "so far".into(),
            first_kept_entry_id: "b1".into(),
            retained_user_entry_ids: vec!["u1".into()],
            tokens_before: 9,
            details: None,
            usage: Default::default(),
        };
        let entries = [
            entry("root", None, Meta, None, prompt),
            entry("u1", Some("root"), Main, None, asked("one")),
            entry("a1", Some("u1"), Main, None, said(10, StopReason::ToolUse)),
            entry("model", Some("a1"), Main, None, model),
            entry("u2", Some("model"), Main, None, asked("two")),
            entry("a2", Some("u2"), Main, None, said(99, StopReason::Aborted)),
            entry("spawn", Some("a2"), Sub, Some(1), spawn),
            entry("s1", Some("spawn"), Sub, Some(1), asked("look")),
            entry("s2", Some("s1"), Sub, Some(1), said(5, StopReason::Stop)),
            entry("end", Some("s2"), Sub, Some(1), end),
            // A branch: `a1` is no longer its thread's last entry.
            entry("b1", Some("a1"), Main, None, asked("instead")),
            entry("b2", Some("b1"), Main, None, said(20, StopReason::Stop)),
            entry("kept", Some("b2"), Main, None, compaction),
        ];
        let refs: Vec<&LogEntry> = entries.iter().collect();
        let annotations = SessionAnnotations {
            tag: Some("label".into()),
            archived: true,
        };
        let created = store.create_session("s", &refs, time(), &annotations);
        let landed = created.unwrap().landed;
        let (sub, fork) = (landed[6].thread, landed[10].thread);
        assert_eq!(store.threads("s").unwrap().len(), 3);
        for (owner, id) in [(WorkOwner::Main(fork), 3), (WorkOwner::Sub(sub), 5)] {
            let pending = PendingRow {
                id: format!("pending-{id}"),
                kind: "follow_up".into(),
                body: "later".into(),
            };
            store.save_pending("s", owner, &pending).unwrap();
            let task = TaskRow {
                id,
                call_id: format!("call-{id}"),
                command: "sleep 100".into(),
                pid: 4242,
                process_start: 1,
                output_path: "out".into(),
                exit_path: "exit".into(),
                started_at: time(),
            };
            store.insert_task("s", owner, &task).unwrap();
        }
    }

    /// What `sql` reads through, as SQLite plans it.
    fn plan(store: &SqliteStore, sql: &str) -> String {
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut statement = store.connection.prepare(&explain).unwrap();
        let unbound = vec![rusqlite::types::Null; statement.parameter_count()];
        let steps = statement.query_map(rusqlite::params_from_iter(unbound), |row| {
            row.get::<_, String>(3)
        });
        let steps: Vec<String> = steps.unwrap().collect::<Result<_, _>>().unwrap();
        steps.join("; ")
    }

    /// Each read of a branch goes through the index made for it and none
    /// sorts: a sort would read every row before the first is returned.
    #[test]
    fn the_reads_of_a_branch_use_their_indexes_and_sort_nothing() {
        let store = SqliteStore::in_memory("p").unwrap();
        for (sql, index) in [
            (walk_sql(true), "COVERING INDEX entries_path"),
            (walk_sql(false), "COVERING INDEX entries_path"),
            (entries_sql(false), "INDEX entries_path"),
            (entries_sql(true), "INDEX entries_state"),
            (USAGE_SQL.to_owned(), "INDEX entries_usage"),
        ] {
            let plan = plan(&store, &sql);
            assert!(plan.contains(index), "{sql}: {plan}");
            assert!(!plan.contains("TEMP B-TREE"), "{sql}: {plan}");
        }
    }

    /// The state entries before a window are chosen by the list of types
    /// in the query. The index only makes that fast: without it, as for a
    /// build whose list has grown, the same entries are read.
    #[test]
    fn state_entries_are_read_the_same_without_their_index() {
        let mut store = SqliteStore::in_memory("p").unwrap();
        a_session_with_everything(&mut store);
        let read = |store: &SqliteStore| -> Vec<(ThreadId, Vec<String>)> {
            let threads = store.threads("s").unwrap();
            let state = threads.iter().map(|row| {
                let span = Span {
                    thread: row.id,
                    after: 0,
                    upto: u64::MAX,
                };
                let records = store.entries(span, true).unwrap();
                let ids = records.into_iter().map(|(_, entry)| entry.id);
                (row.id, ids.collect())
            });
            state.collect()
        };
        let indexed = read(&store);
        let ids: Vec<&[String]> = indexed.iter().map(|(_, ids)| ids.as_slice()).collect();
        assert_eq!(ids[0], ["root", "model"]);
        assert_eq!(ids[1], ["spawn", "end"]);
        assert_eq!(ids[2], ["kept"]);
        store
            .connection
            .execute_batch("DROP INDEX entries_state")
            .unwrap();
        assert!(plan(&store, &entries_sql(true)).contains("entries_path"));
        assert_eq!(read(&store), indexed);
    }

    /// The usage read off the index is what each entry says a completed
    /// response reported, and nothing for any other entry.
    #[test]
    fn the_usage_index_reports_what_the_entries_say() {
        let mut store = SqliteStore::in_memory("p").unwrap();
        a_session_with_everything(&mut store);
        let (threads, records) = store.whole("s").unwrap();
        let mut indexed = Vec::new();
        for row in &threads {
            let span = Span {
                thread: row.id,
                after: 0,
                upto: u64::MAX,
            };
            let reported = |seq, usage: aj_models::types::Usage| indexed.push((seq, usage.input));
            store.usage(span, reported).unwrap();
        }
        indexed.sort();
        let said = (1..).zip(&records).filter_map(|(seq, (_, entry))| {
            let usage = entry.entry.reported_usage()?;
            Some((seq, usage.input))
        });
        let said: Vec<(u64, u64)> = said.collect();
        // The response that was cut off reported 99 and is not counted.
        assert_eq!(said, [(3, 10), (9, 5), (12, 20)]);
        assert_eq!(indexed, said);
    }

    /// A walk visits the rows of its spans in append order or against it,
    /// whatever threads they are of and however their positions
    /// interleave, keeps one type when asked to, and stops when told.
    #[test]
    fn a_walk_merges_its_spans_in_order() {
        let none = SessionAnnotations::default();
        let mut dice = Dice(7);
        for round in 0..6 {
            let mut store = SqliteStore::in_memory("p").unwrap();
            let entries = random_session(&mut dice, 40 + round * 150);
            let refs: Vec<&LogEntry> = entries.iter().collect();
            store.create_session("s", &refs, time(), &none).unwrap();
            let filed = filed_under(&store, "s");
            let threads = store.threads("s").unwrap();
            let last = u64::try_from(filed.len()).unwrap();
            // Some threads whole, some in part, some left out.
            let mut spans = Vec::new();
            for row in &threads {
                let (after, upto) = match dice.roll(4) {
                    0 => continue,
                    1 => (0, u64::MAX),
                    _ => {
                        let after = u64::try_from(dice.roll(filed.len())).unwrap();
                        (
                            after,
                            after + u64::try_from(dice.roll(filed.len())).unwrap(),
                        )
                    }
                };
                spans.push(Span {
                    thread: row.id,
                    after,
                    upto,
                });
            }
            let mut expected: Vec<(u64, ThreadId)> = (1..=last)
                .zip(filed.iter().copied())
                .filter(|(seq, thread)| {
                    let mut spans = spans.iter();
                    spans.any(|span| {
                        span.thread == *thread && span.after < *seq && *seq <= span.upto
                    })
                })
                .collect();
            for descending in [false, true] {
                if descending {
                    expected.reverse();
                }
                let walked = |kind: Option<&str>, stop: usize| {
                    let mut seen = Vec::new();
                    let visit = |row: Shape<'_>| {
                        assert_eq!(row.kind, "internal_context");
                        seen.push((row.seq, row.thread));
                        seen.len() < stop
                    };
                    store.walk(&spans, descending, kind, visit).unwrap();
                    seen
                };
                assert_eq!(walked(None, usize::MAX), expected, "round {round}");
                let some = Some("internal_context");
                assert_eq!(walked(some, usize::MAX), expected, "round {round}");
                assert_eq!(walked(Some("user"), usize::MAX), [], "round {round}");
                let few = expected.len().min(20);
                assert_eq!(walked(None, few.max(1)), expected[..few], "round {round}");
            }
        }
    }

    /// The fingerprint a database carries after each released migration.
    /// A released migration that is edited, in a byte, fails here and not
    /// when a user opens a database. A new migration adds a line. No line
    /// is ever changed.
    #[test]
    fn released_migrations_keep_their_fingerprints() {
        const RELEASED: &[(&str, i64)] = &[("001-initial", 916_791_561)];
        let pinned: Vec<(&str, i64)> = (1..=MIGRATIONS.len())
            .map(|applied| {
                (
                    MIGRATIONS[applied - 1].0,
                    fingerprint(&MIGRATIONS[..applied]),
                )
            })
            .collect();
        assert_eq!(pinned, RELEASED);
    }

    #[test]
    fn a_database_this_build_cannot_read_is_refused_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.sqlite3");
        drop(open(dir.path(), "p"));
        assert_eq!(check_database(&path).unwrap(), Vec::<String>::new());
        // A newer build has applied migrations this one does not know.
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 999).unwrap();
        drop(connection);
        assert!(matches!(
            SqliteStore::open_project(&path, "p"),
            Err(SqliteError::Version(999))
        ));
        assert!(matches!(
            SqliteStore::open_project_read_only(&path, "p"),
            Err(SqliteError::Version(999))
        ));
        let problems = check_database(&path).unwrap();
        assert!(
            problems.len() == 1 && problems[0].contains("999"),
            "{problems:?}"
        );

        // The right count of migrations whose text was not this build's:
        // another lineage's build applied them.
        let edited = dir.path().join("edited.sqlite3");
        drop(SqliteStore::open_project(&edited, "p").unwrap());
        let connection = Connection::open(&edited).unwrap();
        connection.pragma_update(None, "application_id", 7).unwrap();
        drop(connection);
        assert!(matches!(
            SqliteStore::open_project(&edited, "p"),
            Err(SqliteError::Format)
        ));
        assert!(matches!(
            SqliteStore::open_project_read_only(&edited, "p"),
            Err(SqliteError::Format)
        ));
        assert_eq!(check_database(&edited).unwrap().len(), 1);

        // Tables without a migration count are not this build's either.
        let foreign = dir.path().join("foreign.sqlite3");
        let connection = Connection::open(&foreign).unwrap();
        connection
            .execute_batch("CREATE TABLE sessions (id TEXT)")
            .unwrap();
        drop(connection);
        assert!(matches!(
            SqliteStore::open_project(&foreign, "p"),
            Err(SqliteError::Version(0))
        ));
        let tables: i64 = Connection::open(&foreign)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sqlite_master", [], |row| row.get(0))
            .unwrap();
        assert_eq!(tables, 1, "nothing was added to it");
    }
}
