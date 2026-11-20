//! Task records: the durable form of the background tasks, the commands
//! that outlive the process.
//!
//! A task is recorded before its command runs, so none is running without
//! a row a later process can find it by.
//!
//! A task's row is removed when the command is seen to end, in the
//! transaction that saves its task notification. A crash before that
//! transaction leaves the row, and the next process to select its branch
//! looks at the command again. A crash after it leaves the notification,
//! which is delivered once as other pending work is (see
//! [`crate::PendingStore`]).
//!
//! A command its tool call waits for ends with the process and has no row.
//!
//! A row belongs to the thread of the agent that started the
//! command, by the rule pending work follows. A command runs in a turn,
//! and a turn runs on a thread, so its agent always has one.
//!
//! A write the database was too busy to take changed nothing and reports
//! [`SessionError::is_busy`].

use std::path::PathBuf;

use aj_agent::events::AgentId;
use aj_agent::message::AgentMessage;
use chrono::{DateTime, Utc};

use crate::SessionError;
use crate::sqlite::{PendingRow, SqliteError, TaskRow, ThreadId};

/// A task record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRecord {
    /// The task id users see, unique in the session.
    pub id: usize,
    /// The agent that started the command and receives its notification.
    pub owner: AgentId,
    /// The tool call that launched the command.
    pub call_id: String,
    pub command: String,
    /// The process that runs the command.
    pub pid: u32,
    /// When that process started, on the system's own clock. It tells the
    /// process from a later one with the same pid.
    pub process_start: u64,
    pub output_path: PathBuf,
    pub exit_path: PathBuf,
    pub started_at: DateTime<Utc>,
}

/// One session's task records, on the session log's connection. The
/// session's owner holds its writer lock for this handle's lifetime.
pub struct TaskRecords {
    store: crate::log::SharedStore,
    session_id: String,
}

impl TaskRecords {
    pub(crate) fn new(store: crate::log::SharedStore, session_id: String) -> Self {
        Self { store, session_id }
    }

    /// The highest task id this session minted, zero when it has minted
    /// none.
    pub fn last_id(&self) -> Result<usize, SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let last = match &writer.store {
            Some(store) => store.last_task(&self.session_id)?,
            None => 0,
        };
        Ok(usize::try_from(last).unwrap_or(usize::MAX))
    }

    /// Note that task id `id` is taken. A session that is not stored
    /// cannot note it.
    pub fn reserve(&self, id: usize) -> Result<(), SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let session = &self.session_id;
        Ok(writer.writable(session)?.reserve_task(session, wide(id))?)
    }

    /// Record a running task. `Ok` means a later process will find it.
    /// An owner without a thread is an error.
    pub fn start(&self, task: &TaskRecord) -> Result<(), SessionError> {
        let row = TaskRow {
            id: wide(task.id),
            call_id: task.call_id.clone(),
            command: task.command.clone(),
            pid: task.pid,
            process_start: task.process_start,
            output_path: path_text(&task.output_path)?,
            exit_path: path_text(&task.exit_path)?,
            started_at: task.started_at,
        };
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let (session, owner) = (&self.session_id, writer.owner(task.owner)?);
        Ok(writer
            .writable(session)?
            .insert_task(session, owner, &row)?)
    }

    /// End task `id` of `owner`: remove its record, if it has one, and save
    /// its notification in one transaction. On an error neither happened.
    /// A task without a record, a sub-agent run for example, only has its
    /// notification saved, as [`crate::PendingStore`] saves one.
    pub fn finish(
        &self,
        id: usize,
        owner: AgentId,
        notification: &AgentMessage,
    ) -> Result<(), SessionError> {
        let row = PendingRow {
            id: notification.id().to_owned(),
            kind: "task_notification".into(),
            body: serde_json::to_string(notification)?,
        };
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let (session, owner) = (&self.session_id, writer.owner(owner)?);
        let store = writer.writable(session)?;
        Ok(store.finish_task(session, owner, wide(id), &row)?)
    }

    /// The running tasks recorded for `owner`, whose thread the store
    /// holds as `thread_id`, in id order.
    pub(crate) fn restore(
        &self,
        owner: AgentId,
        thread_id: ThreadId,
    ) -> Result<Vec<TaskRecord>, SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let rows = match &writer.store {
            Some(store) => store.tasks(&self.session_id, thread_id)?,
            None => Vec::new(),
        };
        Ok(rows
            .into_iter()
            .map(|row| TaskRecord {
                id: usize::try_from(row.id).unwrap_or(usize::MAX),
                owner,
                call_id: row.call_id,
                command: row.command,
                pid: row.pid,
                process_start: row.process_start,
                output_path: row.output_path.into(),
                exit_path: row.exit_path.into(),
                started_at: row.started_at,
            })
            .collect())
    }
}

fn wide(id: usize) -> u64 {
    u64::try_from(id).unwrap_or(u64::MAX)
}

fn path_text(path: &std::path::Path) -> Result<String, SqliteError> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| SqliteError::Invalid(format!("{path:?} is not valid UTF-8")))
}
