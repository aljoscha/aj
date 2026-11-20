//! Durable pending work: accepted user input and task notifications
//! that were not yet delivered.
//!
//! Each item is saved under the id of the entry it becomes. The
//! append that delivers the entry removes the saved item in the same
//! transaction, so a crash between an in-memory drain and that append keeps
//! the item pending, and a completed append cannot deliver it twice.
//!
//! Pending work belongs to the thread of the agent it is for: the one
//! the main agent is on, a sub-agent's own for that sub-agent. The main
//! agent has none on a draft, a session or branch the store does not have.
//! Its first input there is accepted by
//! [`crate::SessionLog::accept`], which creates the thread and
//! saves the input in one transaction. So every save here finds a
//! thread, and one that does not is an error, not a state to wait
//! out. An agent's saved work is restored when a later process opens the
//! session on a branch that shows its thread.
//!
//! A save or a withdrawal the database was too busy to take changed nothing
//! and reports [`SessionError::is_busy`].
//!
//! # How an input's content is stored
//!
//! The `body` column is text. A message that is one block of text is stored
//! as that text. Any other message, one with an attachment for example, is
//! stored as [`CONTENT_MARK`] followed by its content blocks as JSON. A
//! text that itself starts with the mark is stored the second way, so the
//! two forms cannot be confused.

use aj_agent::events::AgentId;
use aj_agent::message::AgentMessage;
use aj_agent::queue::{PendingInput, PendingKind};
use aj_models::types::UserContent;

use crate::SessionError;
use crate::sqlite::{PendingRow, SqliteError, ThreadId};

/// What starts the body of a saved input that is stored as content blocks
/// (see the module docs). A line feed, which no trimmed text starts with.
const CONTENT_MARK: &str = "\n";

/// The row `input` is saved as (see the module docs).
pub(crate) fn input_row(
    kind: PendingKind,
    input: &PendingInput,
) -> Result<PendingRow, SqliteError> {
    Ok(PendingRow {
        id: input.id.clone(),
        kind: match kind {
            PendingKind::Steering => "steering".into(),
            PendingKind::FollowUp => "follow_up".into(),
        },
        body: input_body(&input.content)?,
    })
}

/// The `body` an input's content is stored as.
fn input_body(content: &[UserContent]) -> Result<String, serde_json::Error> {
    match content {
        [UserContent::Text(only)] if !only.text.starts_with(CONTENT_MARK) => Ok(only.text.clone()),
        blocks => Ok(format!("{CONTENT_MARK}{}", serde_json::to_string(blocks)?)),
    }
}

/// The content a stored `body` holds.
fn input_content(body: String) -> Result<Vec<UserContent>, serde_json::Error> {
    match body.strip_prefix(CONTENT_MARK) {
        Some(blocks) => serde_json::from_str(blocks),
        None => Ok(vec![UserContent::text(body)]),
    }
}

/// Pending work saved for a branch and not yet delivered.
#[derive(Debug)]
pub enum Pending {
    Input(AgentId, PendingKind, PendingInput),
    TaskNotification(AgentId, AgentMessage),
}

/// One session's pending-work store, on the session log's connection. The
/// session's owner holds its writer lock for this handle's lifetime.
pub struct PendingStore {
    store: crate::log::SharedStore,
    session_id: String,
}

impl PendingStore {
    pub(crate) fn new(store: crate::log::SharedStore, session_id: String) -> Self {
        Self { store, session_id }
    }

    /// Save `agent`'s pending input with its attachments. Coalescing
    /// keeps the id, so this replaces the earlier copy. `Ok` means the
    /// input is saved, or was delivered meanwhile.
    pub fn save_input(
        &self,
        agent: AgentId,
        kind: PendingKind,
        input: &PendingInput,
    ) -> Result<(), SessionError> {
        self.save(agent, &input_row(kind, input)?)
    }

    /// Save a task notification for `owner` that no task record accounts
    /// for, under the notification's id. Saving it again replaces the
    /// earlier copy, and one whose entry the session holds was delivered
    /// and is not saved again. So a caller that derives the id from what
    /// the notification reports can repeat the save without the
    /// notification being delivered twice.
    pub fn save_task_notification(
        &self,
        owner: AgentId,
        notification: &AgentMessage,
    ) -> Result<(), SessionError> {
        let row = PendingRow {
            id: notification.id().to_owned(),
            kind: "task_notification".into(),
            body: serde_json::to_string(notification)?,
        };
        self.save(owner, &row)
    }

    /// An owner without a thread is an error (see the module docs).
    fn save(&self, owner: AgentId, row: &PendingRow) -> Result<(), SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        let (id, owner) = (&self.session_id, writer.owner(owner)?);
        Ok(writer.writable(id)?.save_pending(id, owner, row)?)
    }

    /// Whether `agent` has a thread to save work under. The main
    /// agent has none on a draft, where its first input is accepted by
    /// [`crate::SessionLog::accept`].
    pub fn has_thread(&self, agent: AgentId) -> bool {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        writer.thread(agent).is_some()
    }

    /// Drop the saved work with this id, as when the user withdraws a
    /// pending input. A session that is not stored has none.
    pub fn forget(&self, id: &str) -> Result<(), SessionError> {
        let writer = self.store.lock().expect("SQLite mutex poisoned");
        if !writer.stored {
            return Ok(());
        }
        let session = &self.session_id;
        Ok(writer.writable(session)?.delete_pending(session, id)?)
    }

    /// The saved work of `owner`, whose thread the store holds as
    /// `thread_id`, in the order it was saved. An item this build
    /// cannot decode is skipped rather than blocking the session.
    pub(crate) fn restore(
        &self,
        owner: AgentId,
        thread_id: ThreadId,
    ) -> Result<Vec<Pending>, SessionError> {
        let rows = {
            let writer = self.store.lock().expect("SQLite mutex poisoned");
            match &writer.store {
                Some(store) => store.pending(&self.session_id, thread_id)?,
                None => Vec::new(),
            }
        };
        let restore = |row: PendingRow| match row.kind.as_str() {
            "task_notification" => match serde_json::from_str::<AgentMessage>(&row.body) {
                Ok(mut message) => {
                    message.set_id(row.id);
                    Some(Pending::TaskNotification(owner, message))
                }
                Err(error) => {
                    tracing::warn!("skipping an unreadable saved task notification: {error}");
                    None
                }
            },
            kind => {
                let kind = if kind == "steering" {
                    PendingKind::Steering
                } else {
                    PendingKind::FollowUp
                };
                match input_content(row.body) {
                    Ok(content) => {
                        let input = PendingInput {
                            id: row.id,
                            content,
                        };
                        Some(Pending::Input(owner, kind, input))
                    }
                    Err(error) => {
                        tracing::warn!("skipping an unreadable saved input: {error}");
                        None
                    }
                }
            }
        };
        Ok(rows.into_iter().filter_map(restore).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Text is stored as itself. Everything else round-trips through the
    /// marked form, a text that starts with the mark included.
    #[test]
    fn an_input_body_reads_back_as_the_content_it_was_written_from() {
        let read = |content: &[UserContent]| {
            let body = input_body(content).expect("a body");
            (body.clone(), input_content(body).expect("content"))
        };

        let (body, content) = read(&[UserContent::text("plain { \"text\" }")]);
        assert_eq!(body, "plain { \"text\" }");
        assert_eq!(content, [UserContent::text("plain { \"text\" }")]);

        let attached = [
            UserContent::text("look"),
            UserContent::image("aGk=", "image/png"),
            UserContent::text("<file>a</file>"),
        ];
        let (body, content) = read(&attached);
        assert!(body.starts_with(CONTENT_MARK), "{body:?}");
        assert_eq!(content, attached);

        let marked = [UserContent::text("\n[] looks like the marked form")];
        let (body, content) = read(&marked);
        assert_ne!(body, "\n[] looks like the marked form");
        assert_eq!(content, marked);
    }
}
