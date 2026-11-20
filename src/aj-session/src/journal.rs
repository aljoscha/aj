//! The one path by which a hosted session's log is written.
//!
//! Every write goes through [`JournalGuard::record`], under the log lock:
//! it appends, then sends what the write publishes to the journal's
//! [`Journal::stream`], still under that lock. The stream's order is
//! therefore the log's order, and whoever reads the stream learns of every
//! entry from it and from nowhere else.
//!
//! A session installs its [`Journal`] on the agent's event bus as the
//! [`Committer`]. The bus awaits it before it publishes an event, so a
//! message is in the store before the agent holds it and before any
//! listener or client sees it, and a refused commit stops the turn.
//!
//! What an event records:
//!
//! - `MessageEnd`: the message, on its agent's thread.
//! - `AgentStart`: that the agent's turn is running (see
//!   [`crate::TurnStore`]). Nothing is appended.
//! - `SubAgentStart`: the sub-agent's spawn record, which starts its
//!   thread at the main thread's current head.
//! - `SubAgentEnd`: how the run ended.
//! - `CodeModeStore`: the committed writes.
//! - A failed `CompactionEnd`: a transcript-only error.
//!
//! A successful `CompactionEnd` records nothing by itself. The compaction
//! run records its checkpoint and that event in one write, and the bus
//! delivers the event to its listeners afterwards. The committer therefore
//! lets it pass without sending it to the stream a second time.
//!
//! Writes that no agent event stands for (a settings change, a goal
//! entry) call [`Journal::record`] themselves.
//!
//! The entries of a draft are not stored, so nothing is published for them
//! until the draft is created: by the first input
//! ([`JournalGuard::accept`]) or by the first write of work. That write
//! publishes their positions with its own.
//!
//! A write that finds the session database busy has changed nothing, so
//! the journal makes it again, pausing in between without holding a
//! thread, until it lands or [`BUSY_PATIENCE`] has passed. Then the write
//! fails with the busy error. That refuses the one action it was for, a
//! turn for example, and the log stays writable.

use std::future::Future;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use aj_agent::bus::Committer;
use aj_agent::error::BoxError;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::message::AgentMessage;
use aj_models::types::Message;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex as TokioMutex, MutexGuard as TokioMutexGuard};

use crate::log::{AgentLog, EntryRef, SessionError, SessionLog, ThreadFilter};
use crate::replay::TaggedEvent;

/// How long one try of a write waits for another connection's lock on the
/// session database. The wait blocks the thread that runs the write, so it
/// is short, and the pauses between tries do the waiting.
const BUSY_WAIT: Duration = Duration::from_millis(50);

/// The pause before a busy write is tried again. It doubles from the first
/// value up to the second.
const BUSY_PAUSE: (Duration, Duration) = (Duration::from_millis(25), Duration::from_millis(200));

/// How long a busy write is tried for, counted from its first try. A
/// writer that holds the database longer than this, a bulk import of a
/// very large session for example, fails the write.
pub const BUSY_PATIENCE: Duration = Duration::from_secs(5);

/// One item of a journal's stream, in commit order.
#[derive(Debug, Clone)]
pub enum Committed {
    /// An event to publish. It is tagged with the entry it stands for when
    /// it is durable, and that entry is already stored.
    Event(TaggedEvent),
    /// The log's position after a write whose newest entry no event
    /// stands for. Every entry up to it is stored, and none of them has
    /// anything to show.
    Position(u64),
}

/// Writes a session's log and streams what each write publishes.
///
/// Clones share one journal.
#[derive(Clone)]
pub struct Journal(Arc<JournalInner>);

struct JournalInner {
    log: Arc<TokioMutex<SessionLog>>,
    /// Where every write's items go once it is committed. Unbounded: the
    /// send happens under the log guard, where a blocking send would stall
    /// every append of the session. Flow control belongs downstream.
    stream: StdMutex<Option<UnboundedSender<Committed>>>,
    /// Held across each write, so [`Journal::close`] returns with no
    /// write in progress and none to come. It is not the log's own lock:
    /// a write that waits for the log has appended nothing, and a close
    /// must not wait for it.
    closed: StdMutex<bool>,
    /// The log's write connection, which the handles for pending work,
    /// task records and the turn state write through beside the journal.
    writer: crate::log::SharedStore,
}

/// The log lock of a journal's session, taken through the journal.
///
/// Reads go through `Deref`. Appends go through [`Self::record`], which is
/// what publishes them.
pub struct JournalGuard<'a> {
    journal: &'a Journal,
    log: TokioMutexGuard<'a, SessionLog>,
}

impl Journal {
    /// The journal of `log`, which nothing holds locked yet.
    pub fn new(log: Arc<TokioMutex<SessionLog>>) -> Self {
        let writer = log
            .try_lock()
            .expect("a journal is made for a log nothing holds yet")
            .writer();
        Self(Arc::new(JournalInner {
            log,
            stream: StdMutex::new(None),
            closed: StdMutex::new(false),
            writer,
        }))
    }

    /// This journal as the committer of an event bus.
    pub fn committer(&self) -> Arc<dyn Committer> {
        Arc::new(self.clone())
    }

    /// Send what every write from now on publishes to the returned stream.
    /// Whatever the receiver gets is already stored, and positions arrive
    /// in strictly increasing order.
    ///
    /// A dropped receiver is ignored, so an absent consumer never fails a
    /// write.
    pub fn stream(&self) -> UnboundedReceiver<Committed> {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        self.stream_into(tx);
        rx
    }

    /// [`Self::stream`] into a channel the caller made.
    pub fn stream_into(&self, sink: UnboundedSender<Committed>) {
        *self.0.stream.lock().expect("journal stream mutex poisoned") = Some(sink);
    }

    /// Close the session for writing. Every later write is refused: an
    /// append through this journal, and a write through any handle on the
    /// log's connection (pending work, task records, the turn state).
    /// Returns with no write in progress, so the store then holds exactly
    /// what it will hold when the session is next opened.
    ///
    /// This is how a host lets go of a session. Whatever still runs for
    /// the session is stopped afterwards and can write nothing, so the
    /// host leaves what a host that died at this moment leaves. It also
    /// keeps a task that outlives the session from writing after another
    /// host has taken the session.
    ///
    /// A refused write is an error to its caller, which was therefore
    /// told nothing was saved.
    pub fn close(&self) {
        *self.0.closed.lock().expect("journal mutex poisoned") = true;
        // Each handle writes under this lock, so none is mid-write now.
        self.0.writer.lock().expect("SQLite mutex poisoned").closed = true;
    }

    pub fn is_closed(&self) -> bool {
        *self.0.closed.lock().expect("journal mutex poisoned")
    }

    /// Take the session's log lock. For a caller whose write spans awaits
    /// or needs to read first. Anything else calls [`Self::record`].
    pub async fn lock(&self) -> JournalGuard<'_> {
        JournalGuard {
            journal: self,
            log: self.0.log.lock().await,
        }
    }

    /// Take the log lock and [`JournalGuard::record`] one write.
    pub async fn record<T>(
        &self,
        write: impl FnMut(&mut SessionLog) -> Result<(T, Vec<TaggedEvent>), SessionError>,
    ) -> Result<T, SessionError> {
        self.lock().await.record(write).await
    }

    /// Publish one write of `log`, which stored the entries after position
    /// `before`: send `events` to the stream, then the position of the last
    /// stored entry when no event stands for it. The entries of a draft
    /// are not stored, so their positions are published when the draft is
    /// created.
    fn publish(&self, log: &SessionLog, before: u64, events: Vec<TaggedEvent>) {
        let stored = log.stored_seq();
        let mut published = before;
        for event in events {
            if let Some(entry) = &event.entry {
                debug_assert!(
                    published < entry.seq && entry.seq <= log.last_seq(),
                    "a write tags its own entries, in append order: {} after {published}",
                    entry.seq,
                );
                published = entry.seq;
            }
            self.send(Committed::Event(event));
        }
        if published < stored {
            self.send(Committed::Position(stored));
        }
    }

    /// Send one item to the stream, if there is one.
    fn send(&self, item: Committed) {
        let stream = self.0.stream.lock().expect("journal stream mutex poisoned");
        if let Some(sink) = stream.as_ref() {
            let _ = sink.send(item);
        }
    }
}

impl JournalGuard<'_> {
    /// Run `write` on the log and publish it: send the events it returns to
    /// the journal's stream, in order, before the log lock can be released.
    ///
    /// A returned event is tagged with the entry it stands for, or untagged
    /// when it only accompanies the write. An entry `write` appended that no
    /// returned event stands for is published as a [`Committed::Position`],
    /// so the stream's reader still learns that the log moved past it.
    ///
    /// A closed journal refuses, and `write` does not run. When `write`
    /// fails, what it had appended before the failure is still published as
    /// a position.
    ///
    /// `write` runs again when it fails because the database is busy (see
    /// [`SessionError::is_busy`]) and has appended nothing, for as
    /// long as the module docs say. So it must leave nothing behind when
    /// it fails. One that had appended before it met a busy database is
    /// not run again, and fails with what it appended in place. The log
    /// lock is held across the pauses, so no other write comes between.
    pub async fn record<T>(
        &mut self,
        mut write: impl FnMut(&mut SessionLog) -> Result<(T, Vec<TaggedEvent>), SessionError>,
    ) -> Result<T, SessionError> {
        let started = tokio::time::Instant::now();
        let mut pause = BUSY_PAUSE.0;
        loop {
            let before = self.log.last_seq();
            match self.attempt(&mut write) {
                Err(error)
                    if error.is_busy()
                        && self.log.last_seq() == before
                        && started.elapsed() + pause <= BUSY_PATIENCE =>
                {
                    tokio::time::sleep(pause).await;
                    pause = (pause * 2).min(BUSY_PAUSE.1);
                }
                result => return result,
            }
        }
    }

    /// One try of [`Self::record`].
    fn attempt<T>(
        &mut self,
        write: &mut impl FnMut(&mut SessionLog) -> Result<(T, Vec<TaggedEvent>), SessionError>,
    ) -> Result<T, SessionError> {
        // Nothing awaits while this guard is held.
        let closed = self
            .journal
            .0
            .closed
            .lock()
            .expect("journal mutex poisoned");
        if *closed {
            return Err(SessionError::Closed);
        }
        let before = self.log.stored_seq();
        let (value, events) = match self.log.with_busy_wait(BUSY_WAIT, write) {
            Ok((value, events)) => (Ok(value), events),
            Err(error) => (Err(error), Vec::new()),
        };
        self.journal.publish(&self.log, before, events);
        drop(closed);
        value
    }

    /// Accept the main agent's first input for a draft and publish what
    /// that stored (see [`SessionLog::accept`]). One try, which waits
    /// in place for a busy database as other saves of pending work do. A
    /// closed journal refuses.
    pub fn accept(
        &mut self,
        kind: aj_agent::queue::PendingKind,
        input: &aj_agent::queue::PendingInput,
    ) -> Result<(), SessionError> {
        let closed = self
            .journal
            .0
            .closed
            .lock()
            .expect("journal mutex poisoned");
        if *closed {
            return Err(SessionError::Closed);
        }
        let before = self.log.stored_seq();
        let accepted = self.log.accept(kind, input);
        self.journal.publish(&self.log, before, Vec::new());
        drop(closed);
        accepted
    }

    /// The log, for a change that appends no entry, such as moving the
    /// head. An append made through this is not published.
    pub fn log_mut(&mut self) -> &mut SessionLog {
        &mut self.log
    }
}

impl Deref for JournalGuard<'_> {
    type Target = SessionLog;

    fn deref(&self) -> &SessionLog {
        &self.log
    }
}

impl Committer for Journal {
    fn commit<'a>(
        &'a self,
        event: &'a AgentEvent,
    ) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + 'a>> {
        Box::pin(async move {
            if !appends(event) {
                // Streaming events are by far the most frequent, so this
                // branch does not take the log lock.
                if !matches!(
                    event,
                    AgentEvent::CompactionEnd {
                        error: None,
                        summary: Some(_),
                        ..
                    }
                ) {
                    self.send(Committed::Event(TaggedEvent {
                        entry: None,
                        event: event.clone(),
                        branch_settings: None,
                    }));
                }
                return Ok(());
            }
            self.record(|log| {
                let entry = persist(log, event)?;
                let branch_settings = match (&entry, event) {
                    (
                        Some(entry),
                        AgentEvent::MessageEnd {
                            agent_id: AgentId::Main,
                            message,
                        },
                    ) if matches!(message.as_stored_wire(), Some(Message::User(_))) => {
                        // The settings on the appended message's parent path,
                        // read under the append's guard.
                        Some(
                            log.core()
                                .parent_of(&entry.id)
                                .map(|parent| log.core().settings_at(parent))
                                .unwrap_or_default(),
                        )
                    }
                    _ => None,
                };
                let event = TaggedEvent {
                    entry,
                    event: event.clone(),
                    branch_settings,
                };
                Ok(((), vec![event]))
            })
            .await?;
            Ok(())
        })
    }
}

/// Whether [`persist`] writes anything for `event`, which is what decides
/// whether the log lock is taken at all. Kept next to `persist` so the two
/// cannot disagree: a mismatch either misses an append or takes the lock
/// for every streaming update.
fn appends(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::AgentStart { .. }
            | AgentEvent::SubAgentStart { .. }
            | AgentEvent::SubAgentEnd { .. }
            | AgentEvent::MessageEnd { .. }
            | AgentEvent::CodeModeStore { .. }
            | AgentEvent::CompactionEnd { error: Some(_), .. }
    )
}

/// Write whatever `event` persists and return the appended entry, or
/// `None` for an event that appends none.
fn persist(log: &mut SessionLog, event: &AgentEvent) -> Result<Option<EntryRef>, SessionError> {
    match event {
        // A turn is stored as working by the event that starts it, before
        // its first entry. The emit fails on a refused save, so the turn
        // does not start. A wake that finds nothing to do emits no start
        // and stores nothing.
        AgentEvent::AgentStart { agent_id, .. } => {
            log.start_turn(*agent_id)?;
            Ok(None)
        }
        AgentEvent::SubAgentStart {
            parent,
            child,
            task,
            tool_name,
            call_id,
            background,
            settings,
        } => {
            let AgentId::Sub(child_n) = child else {
                return Err(SessionError::InvalidAppend(format!(
                    "SubAgentStart with non-Sub child {child:?}"
                )));
            };
            // Anchor the spawn root at the main thread's current
            // head. A sub-agent cannot spawn a sub-agent (the
            // `agent` tool is removed from its toolset), so the
            // parent is always the main thread.
            let parent_head = log.head().cloned().ok_or_else(|| {
                SessionError::InvalidAppend(format!(
                    "SubAgentStart: parent {parent:?} thread has no head entry to anchor child {child:?} at"
                ))
            })?;
            let appended = log.append_subagent_spawn(
                *child_n,
                parent_head,
                task,
                tool_name,
                call_id.as_deref(),
                *background,
                settings,
            )?;
            Ok(Some(appended))
        }
        AgentEvent::SubAgentEnd {
            child: AgentId::Sub(n),
            report,
            conclusion,
            ..
        } => log.append_subagent_end(*n, *conclusion, report),
        AgentEvent::MessageEnd { agent_id, message } => {
            let appended = persist_message(log, *agent_id, message.clone())?;
            Ok(Some(appended))
        }
        AgentEvent::CodeModeStore { agent_id, writes } => AgentLog::of(log, *agent_id)?
            .add_code_mode_store(writes.clone())
            .map(Some),
        AgentEvent::CompactionEnd {
            agent_id,
            reason,
            tokens_before,
            error: Some(error),
            ..
        } => {
            let filter = ThreadFilter::of(*agent_id);
            log.append_compaction_failed(filter, *reason, *tokens_before, error.clone())
                .map(Some)
        }
        _ => Ok(None),
    }
}

/// Append one finalized message to the log on behalf of `agent_id`.
///
/// For [`AgentId::Main`] the new entry anchors at the main thread's
/// explicit `head` (or the system-prompt root for a fresh thread). For
/// [`AgentId::Sub(n)`] it's the sub-agent thread's own `latest_leaf`;
/// the thread is never empty for a legitimately spawned sub-agent
/// because [`AgentEvent::SubAgentStart`] seeds it with a
/// `SubAgentSpawn` entry.
fn persist_message(
    log: &mut SessionLog,
    agent_id: AgentId,
    message: AgentMessage,
) -> Result<EntryRef, SessionError> {
    AgentLog::of(log, agent_id)?.add_message(message)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    use aj_agent::bus::{EventBus, listener_from_sync};
    use aj_agent::events::{AgentEvent, AgentId, AgentSettings, SubAgentConclusion};
    use aj_agent::message::AgentMessage;
    use aj_agent::tool::ToolDetails;
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, TextContent, ToolResultMessage, UserMessage,
    };
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    use super::{Committed, Journal};
    use crate::log::{
        AgentLog, LogEntry, LogEntryKind, SessionLog, SessionSettings, ThreadFilter,
        test_support::fail_sqlite_commits,
    };
    use crate::persistence::ProjectSessions;
    use crate::replay::{TaggedEvent, project_suffix};
    use aj_agent::bus::Committer;

    /// Set up a temp sessions dir + a fresh log with a frozen system
    /// prompt root.
    fn fresh_log() -> (TempDir, Arc<TokioMutex<SessionLog>>) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ProjectSessions::new(dir.path().join("sessions"));
        let mut log = SessionLog::create(&persistence).expect("create log");
        log.set_system_prompt("test system prompt".to_string())
            .expect("set system prompt");
        (dir, Arc::new(TokioMutex::new(log)))
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

    fn tool_result(id: &str, name: &str, body: &str) -> AgentMessage {
        AgentMessage::wire(Message::ToolResult(ToolResultMessage::text(
            id, name, body, false,
        )))
    }

    #[tokio::test]
    async fn a_closed_journal_refuses_a_commit_that_was_waiting_for_the_log() {
        let (_dir, log) = fresh_log();
        let journal = Journal::new(Arc::clone(&log));
        let held = log.lock().await;
        let initial_len = held.len();

        let waiting = {
            let journal = journal.clone();
            tokio::spawn(async move {
                journal
                    .commit(&AgentEvent::MessageEnd {
                        agent_id: AgentId::Main,
                        message: user_msg("waiting for the log"),
                    })
                    .await
                    .map_err(|error| error.to_string())
            })
        };
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        // Close does not wait for the log, which is still held.
        journal.close();
        drop(held);

        waiting
            .await
            .expect("commit task")
            .expect_err("a closed journal refuses the commit");
        assert_eq!(log.lock().await.len(), initial_len);
    }

    #[tokio::test]
    async fn a_failed_commit_signals_the_owner_after_fusing_the_shared_log() {
        let (_dir, log) = fresh_log();
        {
            let mut guard = log.lock().await;
            AgentLog::main(&mut guard)
                .add_message(user_msg("materialize"))
                .expect("initial punctuation");
        }
        fail_sqlite_commits(&mut *log.lock().await);
        let (failure_signal, mut failures) = tokio::sync::oneshot::channel();
        log.lock().await.install_failure_signal(failure_signal);
        let journal = Journal::new(Arc::clone(&log));
        let _events = journal.stream();
        let event = AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("fails partially"),
        };

        journal
            .commit(&event)
            .await
            .expect_err("first append fails");
        let reported = failures.try_recv().expect("first failure signal");
        let fused = log
            .lock()
            .await
            .write_failure()
            .cloned()
            .expect("log fused before listener returned");
        assert_eq!(
            reported, fused,
            "the signal carries the log's exact first failure"
        );

        journal
            .commit(&event)
            .await
            .expect_err("later append sees the fuse");
        assert_eq!(
            log.lock().await.write_failure(),
            Some(&fused),
            "the first failure stays the log's failure"
        );
    }

    /// A second connection that holds the session database's write lock,
    /// as another process's bulk import does.
    fn lock_database(log: &SessionLog) -> rusqlite::Connection {
        let other = rusqlite::Connection::open(log.path()).expect("open the database");
        other
            .execute_batch("BEGIN IMMEDIATE")
            .expect("take the lock");
        other
    }

    #[tokio::test]
    async fn a_busy_commit_is_tried_again_until_the_database_is_free() {
        let (_dir, log) = fresh_log();
        let other = {
            let mut guard = log.lock().await;
            AgentLog::main(&mut guard)
                .add_message(user_msg("stored"))
                .expect("store the session");
            lock_database(&guard)
        };
        let journal = Journal::new(Arc::clone(&log));
        let mut events = journal.stream();
        // Held for longer than one try waits, so the first try is refused.
        let held = super::BUSY_WAIT * 6;
        let release = std::thread::spawn(move || {
            std::thread::sleep(held);
            other.execute_batch("COMMIT").expect("release the lock");
        });
        let event = AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("waits"),
        };
        let started = std::time::Instant::now();
        journal.commit(&event).await.expect("the commit lands");
        assert!(started.elapsed() >= held, "the commit did not have to wait");
        release.join().expect("release thread");

        // It landed once, and was published once.
        let guard = log.lock().await;
        assert!(guard.write_failure().is_none());
        assert_eq!(guard.last_seq(), 3);
        assert!(matches!(
            events.try_recv(),
            Ok(Committed::Event(TaggedEvent { entry: Some(entry), .. })) if entry.seq == 3
        ));
        assert!(events.try_recv().is_err());
        drop(guard);

        // A write outside the journal still waits in place, as long as it
        // did before the journal wrote.
        let mut guard = log.lock().await;
        let other = lock_database(&guard);
        let release = std::thread::spawn(move || {
            std::thread::sleep(held);
            other.execute_batch("COMMIT").expect("release the lock");
        });
        AgentLog::main(&mut guard)
            .add_message(user_msg("direct"))
            .expect("a direct append waits the lock out");
        release.join().expect("release thread");
    }

    #[tokio::test(start_paused = true)]
    async fn a_commit_still_busy_after_the_bound_fails_and_leaves_the_log_writable() {
        let (_dir, log) = fresh_log();
        let (failure_signal, mut failures) = tokio::sync::oneshot::channel();
        let other = {
            let mut guard = log.lock().await;
            AgentLog::main(&mut guard)
                .add_message(user_msg("stored"))
                .expect("store the session");
            guard.install_failure_signal(failure_signal);
            lock_database(&guard)
        };
        let journal = Journal::new(Arc::clone(&log));
        let mut events = journal.stream();
        let event = AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("refused"),
        };

        let started = tokio::time::Instant::now();
        let refused = journal.commit(&event).await.expect_err("still busy");
        let refused = refused
            .downcast_ref::<crate::SessionError>()
            .expect("the log's own error");
        assert!(refused.is_busy(), "{refused}");
        // The pauses between tries are what the clock here counts.
        let waited = started.elapsed();
        let longest_pause = super::BUSY_PAUSE.1;
        assert!(
            waited <= super::BUSY_PATIENCE && waited + longest_pause > super::BUSY_PATIENCE,
            "tried for {waited:?}"
        );
        // Nothing was written or published, and the session goes on.
        assert!(events.try_recv().is_err());
        assert!(failures.try_recv().is_err(), "a busy store ended the log");
        {
            let guard = log.lock().await;
            assert!(guard.write_failure().is_none());
            assert_eq!(guard.last_seq(), 2);
        }

        other.execute_batch("COMMIT").expect("release the lock");
        let event = AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("accepted"),
        };
        journal.commit(&event).await.expect("a later commit lands");
        assert_eq!(log.lock().await.last_seq(), 3);
    }

    #[tokio::test]
    async fn code_mode_store_append_failures_fuse_without_forwarding() {
        let (_dir, log) = fresh_log();
        fail_sqlite_commits(&mut *log.lock().await);
        let journal = Journal::new(Arc::clone(&log));
        let mut events = journal.stream();
        let event = AgentEvent::CodeModeStore {
            agent_id: AgentId::Main,
            writes: [("key".into(), serde_json::json!("value"))].into(),
        };
        journal
            .commit(&event)
            .await
            .expect_err("commit must await durability");
        assert!(
            events.try_recv().is_err(),
            "failed commit must not be forwarded"
        );
        let guard = log.lock().await;
        let fused = guard.write_failure().cloned().expect("the log fused");
        assert!(
            guard.head().is_none(),
            "failed append must not advance the branch"
        );
        drop(guard);
        journal.commit(&event).await.expect_err("fused");
        assert_eq!(log.lock().await.write_failure(), Some(&fused));
    }

    fn count_string(value: &serde_json::Value, target: &str) -> usize {
        match value {
            serde_json::Value::String(text) => usize::from(text == target),
            serde_json::Value::Array(values) => {
                values.iter().map(|value| count_string(value, target)).sum()
            }
            serde_json::Value::Object(object) => object
                .values()
                .map(|value| count_string(value, target))
                .sum(),
            _ => 0,
        }
    }

    /// A SubAgentStart event carrying a representative bundle
    /// identity.
    fn sub_start(n: usize, task: &str) -> AgentEvent {
        AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(n),
            task: task.to_string(),
            tool_name: "agent".into(),
            call_id: Some("call-spawn".into()),
            background: false,
            settings: AgentSettings {
                context_window: 0,
                provider: "anthropic".to_string(),
                model_id: "claude-x".to_string(),
                thinking: "medium".to_string(),
                thinking_display: String::new(),
                speed: "standard".to_string(),
                verbosity: "default".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn user_message_appends_to_empty_user_thread() {
        // First user message on a fresh thread anchors at the
        // SystemPrompt root entry.
        let (_dir, log) = fresh_log();
        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("hi"),
        })
        .await
        .expect("emit");

        let log_guard = log.lock().await;
        let head = log_guard
            .latest_leaf(ThreadFilter::MAIN)
            .expect("main-thread head exists after emit");
        let convo = log_guard.linearize(&head, ThreadFilter::MAIN);
        let last = convo.last_message().expect("at least one message");
        assert!(matches!(last, Message::User(_)));
    }

    /// The event that starts a turn stores it as working, before any of
    /// its entries. A save the store refuses fails the event, so the turn
    /// does not start, and the log stays writable for its owner to find
    /// out why.
    #[tokio::test]
    async fn agent_start_stores_the_turn_as_working_or_fails() {
        let (dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = AgentLog::main(&mut log_guard);
            view.add_message(user_msg("hi")).expect("user msg");
        }
        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());
        let start = || AgentEvent::AgentStart {
            agent_id: AgentId::Main,
            assignment_pending: false,
        };
        let turns = log.lock().await.turn_state();
        assert_eq!(turns.get(AgentId::Main), crate::TurnState::Idle);
        bus.emit(start()).await.expect("emit");
        assert_eq!(turns.get(AgentId::Main), crate::TurnState::Working);
        let session_id = log.lock().await.session_id().to_owned();
        let persistence = ProjectSessions::new(dir.path().join("sessions"));
        // A reopen finds the turn its process did not end.
        let rival = SessionLog::resume(&persistence, &session_id).expect("resume");
        let found = rival.turn_state().get(AgentId::Main);
        assert_eq!(found, crate::TurnState::Interrupted);
        drop(rival);

        // The store refuses the next save of a turn state.
        turns
            .set(AgentId::Main, crate::TurnState::Idle)
            .expect("end");
        let database = log.lock().await.path().to_owned();
        rusqlite::Connection::open(database)
            .expect("open the database")
            .execute_batch(
                "CREATE TRIGGER refuse_turn BEFORE UPDATE OF turn ON threads
                 BEGIN SELECT RAISE(ABORT, 'injected turn write failure'); END;",
            )
            .expect("install the turn fault");
        assert!(bus.emit(start()).await.is_err(), "a refused save");
        assert_eq!(turns.get(AgentId::Main), crate::TurnState::Idle);
    }

    #[tokio::test]
    async fn assistant_message_appends_to_user_thread() {
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = AgentLog::main(&mut log_guard);
            view.add_message(user_msg("hi")).expect("user msg");
        }

        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: assistant_text("hello"),
        })
        .await
        .expect("emit");

        let log_guard = log.lock().await;
        let head = log_guard
            .latest_leaf(ThreadFilter::MAIN)
            .expect("main-thread head exists");
        let convo = log_guard.linearize(&head, ThreadFilter::MAIN);
        let last = convo.last_message().expect("at least one message");
        assert!(matches!(last, Message::Assistant(_)));
    }

    #[tokio::test]
    async fn assistant_account_reaches_the_durable_log() {
        let (dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = AgentLog::main(&mut log_guard);
            view.add_message(user_msg("hi")).expect("user msg");
        }
        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());
        let mut assistant = AssistantMessage::empty();
        assistant.content = vec![AssistantContent::Text(TextContent {
            text: "hello".to_string(),
            text_signature: None,
        })];
        assistant.account = Some("work".to_string());

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::Assistant(assistant)),
        })
        .await
        .expect("emit");

        let log_guard = log.lock().await;
        let head = log_guard
            .latest_leaf(ThreadFilter::MAIN)
            .expect("main-thread head exists");
        let convo = log_guard.linearize(&head, ThreadFilter::MAIN);
        let Some(Message::Assistant(message)) = convo.last_message() else {
            panic!("expected assistant message");
        };
        assert_eq!(message.account.as_deref(), Some("work"));

        // MessageEnd commits immediately. Reading the
        // stored entry pins the durable field name rather than only the
        // in-memory value that the listener received.
        let records = ProjectSessions::new(dir.path().join("sessions"))
            .read_entries(log_guard.session_id())
            .expect("read the stored session");
        let jsonl = serde_json::to_string(&records).expect("records serialize");
        assert!(
            jsonl.contains(r#""account":"work""#),
            "the persisted assistant record must carry the account key: {jsonl}"
        );
    }

    #[tokio::test]
    async fn tool_result_message_appends_to_user_thread() {
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = AgentLog::main(&mut log_guard);
            view.add_message(user_msg("hi")).expect("u");
            // Assistant message carrying a tool call.
            let assistant = AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::ToolCall(aj_models::types::ToolCall {
                    is_raw: false,
                    id: "tu-1".into(),
                    name: "ping".into(),
                    arguments: serde_json::json!({}),
                })],
                ..AssistantMessage::empty()
            }));
            view.add_message(assistant).expect("a");
        }

        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: tool_result("tu-1", "ping", "pong"),
        })
        .await
        .expect("emit tool result");

        let log_guard = log.lock().await;
        let entries: Vec<_> = log_guard.entries_in_order().into_iter().cloned().collect();
        let last = entries.last().expect("log has entries");
        match &last.entry {
            LogEntryKind::Message { message: m } => match m.as_stored_wire() {
                Some(Message::ToolResult(tr)) => {
                    assert_eq!(tr.tool_call_id, "tu-1");
                    assert!(!tr.is_error);
                }
                other => panic!("expected ToolResult wire message, got {other:?}"),
            },
            other => panic!("expected Message entry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn persistence_append_compacts_without_mutating_the_event() {
        let (dir, log) = fresh_log();
        let body = "unique tool result body\n".repeat(30);
        let mut result = ToolResultMessage::text("tu-1", "read_file", &body, false);
        result.details = Some(
            serde_json::to_value(ToolDetails::Text {
                summary: "read_file large.txt".to_string(),
                body: body.clone(),
            })
            .expect("serialize details"),
        );
        let event = AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: AgentMessage::wire(Message::ToolResult(result)),
        };

        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let _capture = bus.subscribe(listener_from_sync(move |event| {
            captured_clone
                .lock()
                .expect("capture mutex")
                .push(event.clone());
        }));

        bus.emit(event).await.expect("emit tool result");

        let session = log.lock().await.session_id().to_owned();
        let tool_entry = ProjectSessions::new(dir.path().join("sessions"))
            .read_entries(&session)
            .expect("read the stored session")
            .iter()
            .map(|entry| serde_json::to_value(entry).expect("entry JSON"))
            .find(|entry| entry["message"]["role"] == "tool_result")
            .expect("tool result entry");
        let persisted = &tool_entry["message"]["details"];
        assert_eq!(persisted["kind"], "text");
        assert_eq!(persisted["summary"], "read_file large.txt");
        assert_eq!(persisted["body_ref"]["source"], "content_text");
        assert_eq!(persisted["body_ref"]["append_newline"], false);
        assert!(persisted.get("body").is_none());
        assert_eq!(
            count_string(&tool_entry, &body),
            1,
            "body stored only in content"
        );

        let captured = captured.lock().expect("capture mutex");
        let live_details = captured.iter().find_map(|event| match event {
            AgentEvent::MessageEnd { message, .. } => match message.as_stored_wire() {
                Some(Message::ToolResult(result)) => result.details.as_ref(),
                _ => None,
            },
            _ => None,
        });
        assert_eq!(
            live_details
                .and_then(|details| details.get("body"))
                .and_then(serde_json::Value::as_str),
            Some(body.as_str()),
            "other subscribers retain full details",
        );
    }

    #[tokio::test]
    async fn sub_agent_start_writes_spawn_entry_anchored_at_parent_head() {
        // `SubAgentStart` must immediately seed the sub thread with
        // one `SubAgentSpawn` entry anchored at the parent's
        // `latest_leaf`; the sub-agent's first `MessageEnd` then
        // chains onto it.
        let (dir, log) = fresh_log();
        let parent_anchor = {
            let mut log_guard = log.lock().await;
            let mut view = AgentLog::main(&mut log_guard);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_text("ack")).expect("a");
            log_guard
                .latest_leaf(ThreadFilter::MAIN)
                .expect("parent anchor exists")
        };

        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());

        let mut start = sub_start(1, "do thing");
        if let AgentEvent::SubAgentStart { tool_name, .. } = &mut start {
            *tool_name = "oracle".into();
        }
        bus.emit(start).await.expect("emit start");

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: user_msg("do it"),
        })
        .await
        .expect("emit user");

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: assistant_text("done"),
        })
        .await
        .expect("emit assistant");

        let session_id = log.lock().await.session_id().to_string();
        let persistence = ProjectSessions::new(dir.path().join("sessions"));
        let resumed = SessionLog::resume(&persistence, &session_id).expect("resume");
        let sub_head = resumed
            .latest_leaf(ThreadFilter::sub(1))
            .expect("sub-agent thread head exists");
        let convo = resumed.linearize(&sub_head, ThreadFilter::sub(1));
        let entries: Vec<_> = convo.entries().to_vec();
        // One spawn entry followed by the two messages.
        assert_eq!(entries.len(), 3, "got entries: {entries:#?}");
        match &entries[0].entry {
            LogEntryKind::SubAgentSpawn {
                task,
                tool_name,
                settings,
                ..
            } => {
                assert_eq!(task, "do thing");
                assert_eq!(tool_name, "oracle");
                assert_eq!(settings.provider, "anthropic");
                assert_eq!(settings.model_id, "claude-x");
                assert_eq!(settings.thinking, "medium");
                assert_eq!(settings.speed, "standard");
            }
            other => panic!("expected SubAgentSpawn, got {other:?}"),
        }
        assert_eq!(entries[0].parent_id.as_ref(), Some(&parent_anchor));
        // The first message chains onto the spawn entry.
        assert_eq!(entries[1].parent_id.as_ref(), Some(&entries[0].id));
        assert!(matches!(entries[1].entry, LogEntryKind::Message { .. }));
        assert_eq!(entries[2].parent_id.as_ref(), Some(&entries[1].id));
        assert!(
            crate::replay::replay(&resumed).any(|event| matches!(
                event,
                AgentEvent::SubAgentStart { tool_name, .. } if tool_name == "oracle"
            )),
            "replay retains the persisted spawning tool"
        );
    }

    #[tokio::test]
    async fn sub_agent_end_records_how_the_run_ended() {
        // Conclusions are not persisted. On resume the outcome is
        // reconstructed from the sub's final message stop reason, so the
        // listener must leave the sub thread's last entry as that message
        // and add nothing for `SubAgentEnd`.
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = AgentLog::main(&mut log_guard);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_text("ack")).expect("a");
        }

        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());

        bus.emit(sub_start(1, "do thing"))
            .await
            .expect("emit start");
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: assistant_text("partial"),
        })
        .await
        .expect("emit assistant");

        bus.emit(AgentEvent::SubAgentEnd {
            parent: AgentId::Main,
            child: AgentId::Sub(1),
            report: "sub-agent failed: boom".into(),
            conclusion: SubAgentConclusion::Failed,
        })
        .await
        .expect("emit end");

        let log_guard = log.lock().await;
        let sub_head = log_guard
            .latest_leaf(ThreadFilter::sub(1))
            .expect("sub-agent thread head exists");
        let convo = log_guard.linearize(&sub_head, ThreadFilter::sub(1));
        // The run's end closes its thread, with what the parent was handed.
        match &convo.entries().last().expect("at least one entry").entry {
            LogEntryKind::SubAgentEnd { conclusion, report } => {
                assert_eq!(*conclusion, SubAgentConclusion::Failed);
                assert_eq!(report, "sub-agent failed: boom");
            }
            other => panic!("expected the run's end record, got {other:?}"),
        }
        // It is not part of what the sub-agent's model reads.
        assert_eq!(convo.agent_messages().len(), 1);
    }

    #[tokio::test]
    async fn sub_agent_continuation_chains_on_existing_subthread() {
        // A re-prompt of a retained sub-agent emits no new
        // `SubAgentStart`; its `MessageEnd` events must chain onto the
        // existing sub-thread leaf, not re-anchor at the parent head.
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = AgentLog::main(&mut log_guard);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_text("ack")).expect("a");
        }

        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());

        // Initial sub-agent run: anchored at the parent head.
        bus.emit(sub_start(1, "do thing"))
            .await
            .expect("emit start");
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: user_msg("do it"),
        })
        .await
        .expect("emit user");
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: assistant_text("done"),
        })
        .await
        .expect("emit assistant");

        // Continuation: no `SubAgentStart`, just more messages.
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: user_msg("more"),
        })
        .await
        .expect("emit continuation user");
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: assistant_text("done again"),
        })
        .await
        .expect("emit continuation assistant");

        let log_guard = log.lock().await;
        let sub_head = log_guard
            .latest_leaf(ThreadFilter::sub(1))
            .expect("sub-agent thread head exists");
        let convo = log_guard.linearize(&sub_head, ThreadFilter::sub(1));
        let entries: Vec<_> = convo.entries().to_vec();

        // One spawn entry + four messages live in a single linear
        // sub-thread, in order: chaining (not re-anchoring) is what
        // keeps the continuation in the same thread after the
        // initial leaf.
        assert_eq!(entries.len(), 5, "got entries: {entries:#?}");
        let texts: Vec<String> = entries[1..].iter().map(entry_text).collect();
        assert_eq!(
            texts,
            vec![
                "do it".to_string(),
                "done".to_string(),
                "more".to_string(),
                "done again".to_string(),
            ]
        );

        // The continuation's first user message ("more") chains onto
        // the prior sub-thread leaf (assistant "done"), not the parent.
        let done = &entries[2];
        let more = &entries[3];
        assert_eq!(more.parent_id.as_ref(), Some(&done.id));
    }

    /// Extract the concatenated text of a wire message entry. Panics on
    /// non-message entries; the sub-thread tests only enqueue messages.
    fn entry_text(entry: &LogEntry) -> String {
        let message = match &entry.entry {
            LogEntryKind::Message { message } => message,
            other => panic!("expected Message entry, got {other:?}"),
        };
        match message.as_stored_wire() {
            Some(Message::User(u)) => u
                .content
                .iter()
                .filter_map(|c| match c {
                    aj_models::types::UserContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect(),
            Some(Message::Assistant(a)) => a
                .content
                .iter()
                .filter_map(|c| match c {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect(),
            other => panic!("expected user/assistant message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sub_agent_assistant_without_anchor_returns_error() {
        // No `SubAgentStart` seeded the sub thread beforehand: the
        // thread has no leaf, so the bus call should fail.
        let (_dir, log) = fresh_log();
        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());

        let err = bus
            .emit(AgentEvent::MessageEnd {
                agent_id: AgentId::Sub(2),
                message: assistant_text("done"),
            })
            .await
            .expect_err("emit should fail when sub-agent thread is empty");
        assert!(err.to_string().contains("no head entry"));
    }

    #[tokio::test]
    async fn non_message_end_events_do_nothing() {
        // MessageStart / MessageUpdate / notices / lifecycle markers
        // flow through the listener as no-ops. The log stays at
        // exactly one entry (the system prompt).
        let (_dir, log) = fresh_log();
        let bus = EventBus::new();
        bus.set_committer(Journal::new(Arc::clone(&log)).committer());

        bus.emit(AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: "ignored".into(),
        })
        .await
        .expect("emit");
        bus.emit(AgentEvent::MessageStart {
            agent_id: AgentId::Main,
            message: user_msg("ignored too"),
        })
        .await
        .expect("emit");

        let log_guard = log.lock().await;
        // Only the system prompt root is present.
        assert_eq!(log_guard.len(), 1);
    }

    /// Drain everything the journal has already sent. The journal
    /// sends inline while the bus awaits it, so by the time an `emit`
    /// returns its event is in the channel.
    ///
    /// Panics on a position: the tests that use this append only through
    /// events, which stand for their entries.
    fn drained(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Committed>) -> Vec<TaggedEvent> {
        let mut out = Vec::new();
        while let Ok(item) = rx.try_recv() {
            match item {
                Committed::Event(event) => out.push(event),
                Committed::Position(seq) => panic!("unexpected bare position {seq}"),
            }
        }
        out
    }

    /// The id of the entry at 1-based position `seq`.
    ///
    /// By index rather than by filtering the entries: a hole in the
    /// append order would shift what a filtered position names, and a tag
    /// is exactly a position.
    async fn entry_id_at(log: &Arc<TokioMutex<SessionLog>>, seq: u64) -> Option<String> {
        let index = usize::try_from(seq).expect("fits usize") - 1;
        log.lock()
            .await
            .core()
            .entry_in_append_order(index)
            .map(|entry| entry.id.clone())
    }

    /// The durable positions the sink saw, in delivery order. Spec section
    /// 5 requires these to be strictly increasing.
    fn durable_seqs(forwarded: &[TaggedEvent]) -> Vec<u64> {
        forwarded
            .iter()
            .filter_map(|tagged| tagged.entry.as_ref().map(|entry| entry.seq))
            .collect()
    }

    #[tokio::test]
    async fn branch_settings_agree_live_and_replayed_without_resolving_missing_axes() {
        use aj_agent::message::{TaskNotification, TaskNotificationKind, TaskOutcome};

        for seeded in [false, true] {
            let (_dir, log) = fresh_log();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let bus = EventBus::new();
            let journal = Journal::new(Arc::clone(&log));
            journal.stream_into(tx);
            bus.set_committer(journal.committer());
            let mut initial = SessionSettings::default();
            if seeded {
                let mut guard = log.lock().await;
                guard
                    .append_model_change(ThreadFilter::MAIN, "provider", "seed", 0)
                    .unwrap();
                guard
                    .append_account_change("provider", Some("work"))
                    .unwrap();
                guard.append_account_change("other", Some("")).unwrap();
                guard
                    .append_thinking_change(ThreadFilter::MAIN, "high")
                    .unwrap();
                initial.model = Some(("provider".into(), "seed".into()));
                initial.accounts = [
                    ("provider".into(), "work".into()),
                    ("other".into(), "".into()),
                ]
                .into();
                initial.thinking = Some("high".into());
            }
            let first = user_msg("first");
            bus.emit(AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: first.clone(),
            })
            .await
            .unwrap();
            bus.emit(AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: AgentMessage::wire(Message::Assistant(AssistantMessage {
                    provider: "provider".into(),
                    model: "actually-used".into(),
                    account: Some("not-a-session-pin".into()),
                    ..AssistantMessage::empty()
                })),
            })
            .await
            .unwrap();
            {
                let mut guard = log.lock().await;
                guard.append_account_change("provider", None).unwrap();
                guard
                    .append_thinking_change(ThreadFilter::MAIN, "off")
                    .unwrap();
                guard
                    .append_speed_change(ThreadFilter::MAIN, "standard")
                    .unwrap();
                guard
                    .append_verbosity_change(ThreadFilter::MAIN, "default")
                    .unwrap();
                guard
                    .append_env_change([("TOKEN".into(), "secret-env-value".into())].into())
                    .unwrap();
            }
            bus.emit(sub_start(1, "child")).await.unwrap();
            for (agent_id, message) in [
                (AgentId::Sub(1), user_msg("child prompt")),
                (AgentId::Sub(1), assistant_text("child answer")),
                (
                    AgentId::Main,
                    AgentMessage::task_notification(TaskNotification::new(
                        "job".into(),
                        TaskNotificationKind::Bash,
                        TaskOutcome::Succeeded,
                        "done".into(),
                    )),
                ),
                (AgentId::Main, tool_result("call", "read_file", "body")),
            ] {
                bus.emit(AgentEvent::MessageEnd { agent_id, message })
                    .await
                    .unwrap();
            }
            let second = user_msg("second");
            bus.emit(AgentEvent::MessageStart {
                agent_id: AgentId::Main,
                message: second.clone(),
            })
            .await
            .unwrap();
            bus.emit(AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: second.clone(),
            })
            .await
            .unwrap();
            let mut expected_second = initial.clone();
            expected_second.model = Some(("provider".into(), "actually-used".into()));
            expected_second.accounts.remove("provider");
            expected_second.thinking = Some("off".into());
            expected_second.speed = Some("standard".into());
            expected_second.verbosity = Some("default".into());

            // Consume only after later settings exist. Delivery-time state must
            // not replace either message's historical parent-path baseline.
            let mut guard = log.lock().await;
            guard
                .append_model_change(ThreadFilter::MAIN, "later", "later", 0)
                .unwrap();
            guard
                .append_thinking_change(ThreadFilter::MAIN, "max")
                .unwrap();
            let resumed = SessionLog::resume(
                &ProjectSessions::new(guard.path().parent().unwrap().to_path_buf()),
                guard.session_id(),
            )
            .unwrap();
            let live = drained(&mut rx);
            let replayed: Vec<_> =
                project_suffix(&resumed.snapshot(), None, &BTreeSet::new()).collect();
            for events in [&live, &replayed] {
                let mut baselines = Vec::new();
                for tagged in events {
                    if let AgentEvent::MessageEnd {
                        agent_id: AgentId::Main,
                        message,
                    } = &tagged.event
                        && matches!(message.as_stored_wire(), Some(Message::User(_)))
                    {
                        let entry = tagged.entry.as_ref().expect("durable user message");
                        let settings = tagged.branch_settings.as_ref().expect("branch baseline");
                        let parent = resumed.core().parent_of(&entry.id).unwrap();
                        assert_eq!(*settings, resumed.settings_at(parent).unwrap());
                        assert_eq!(
                            *settings,
                            resumed.linearize(parent, ThreadFilter::MAIN).settings()
                        );
                        baselines.push((message.id(), settings.clone()));
                    } else {
                        assert_eq!(tagged.branch_settings, None, "{tagged:?}");
                    }
                }
                assert_eq!(
                    baselines,
                    vec![
                        (first.id(), initial.clone()),
                        (second.id(), expected_second.clone())
                    ]
                );
                assert!(!format!("{events:?}").contains("secret-env-value"));
            }
        }
    }

    #[tokio::test]
    async fn forwarder_tags_the_durable_events_and_forwards_the_rest() {
        let (_dir, log) = fresh_log();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = EventBus::new();
        let journal = Journal::new(Arc::clone(&log));
        journal.stream_into(tx);
        bus.set_committer(journal.committer());

        let user = user_msg("hi");
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user.clone(),
        })
        .await
        .expect("emit user message");
        bus.emit(AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: "nothing durable here".into(),
        })
        .await
        .expect("emit notice");
        bus.emit(sub_start(1, "do thing"))
            .await
            .expect("emit sub start");
        let sub_reply = assistant_text("done");
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: sub_reply.clone(),
        })
        .await
        .expect("emit sub message");

        // A compaction run records its checkpoint and the event that stands
        // for it in one write. The bus then delivers that event, which the
        // committer must not send to the stream a second time.
        let first_kept = log
            .lock()
            .await
            .latest_leaf(ThreadFilter::MAIN)
            .expect("user leaf");
        let end = AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason: aj_agent::events::CompactionReason::Manual,
            tokens_before: 100,
            tokens_after: 10,
            usage: None,
            summary: Some("summary".into()),
            error: None,
        };
        journal
            .record(|log| {
                let checkpoint = log.append_compaction(
                    ThreadFilter::MAIN,
                    "summary".into(),
                    first_kept.clone(),
                    Vec::new(),
                    100,
                    None,
                    aj_models::types::Usage::default(),
                )?;
                let event = TaggedEvent {
                    entry: Some(checkpoint),
                    event: end.clone(),
                    branch_settings: None,
                };
                Ok(((), vec![event]))
            })
            .await
            .expect("record the compaction checkpoint");
        bus.emit(end).await.expect("emit compaction end");

        let forwarded = drained(&mut rx);
        let kinds: Vec<(&'static str, Option<u64>)> = forwarded
            .iter()
            .map(|persisted| {
                let kind = match &persisted.event {
                    AgentEvent::MessageEnd { .. } => "message_end",
                    AgentEvent::Notice { .. } => "notice",
                    AgentEvent::SubAgentStart { .. } => "sub_agent_start",
                    AgentEvent::CompactionEnd { .. } => "compaction_end",
                    other => panic!("unexpected forwarded event {other:?}"),
                };
                (kind, persisted.entry.as_ref().map(|entry| entry.seq))
            })
            .collect();
        // Entry 1 is the system prompt, which no event stands for.
        assert_eq!(
            kinds,
            vec![
                ("message_end", Some(2)),
                ("notice", None),
                ("sub_agent_start", Some(3)),
                ("message_end", Some(4)),
                ("compaction_end", Some(5)),
            ],
            "every event is forwarded, exactly the durable ones tagged"
        );

        for tagged in &forwarded {
            assert_eq!(
                tagged.branch_settings,
                matches!(
                    &tagged.event,
                    AgentEvent::MessageEnd {
                        agent_id: AgentId::Main,
                        ..
                    }
                )
                .then(SessionSettings::default),
            );
            let Some(entry) = &tagged.entry else {
                continue;
            };
            assert_eq!(
                Some(entry.id.clone()),
                entry_id_at(&log, entry.seq).await,
                "tag names its own entry"
            );
        }
        assert_eq!(
            durable_seqs(&forwarded),
            vec![2, 3, 4, 5],
            "durable positions reach the sink in increasing order"
        );
        // A `MessageEnd`'s tag is the message's own id: that is what lets a
        // remote client rebuild the id its reducer keys transcript entries
        // and branch targets on.
        assert_eq!(
            forwarded[0].entry.as_ref().map(|e| e.id.as_str()),
            Some(user.id())
        );
        assert_eq!(
            forwarded[3].entry.as_ref().map(|e| e.id.as_str()),
            Some(sub_reply.id())
        );
        // The `SubAgentStart` tag is the spawn root the listener wrote.
        let spawn_id = forwarded[2]
            .entry
            .as_ref()
            .map(|entry| entry.id.clone())
            .expect("spawn root tagged");
        let log_guard = log.lock().await;
        let spawn_entry = log_guard
            .entries_in_order()
            .into_iter()
            .find(|entry| entry.id == spawn_id)
            .cloned()
            .expect("spawn root in the log");
        assert!(matches!(
            spawn_entry.entry,
            LogEntryKind::SubAgentSpawn { .. }
        ));
        let before = log_guard.linearize(log_guard.head().unwrap(), ThreadFilter::MAIN);
        assert!(crate::compaction::prepare_compaction(&before, 1).is_none());
        assert_eq!(log_guard.stats().unwrap().compactions, 1);
        let messages = serde_json::to_value(before.messages()).unwrap();
        let occupancy = crate::compaction::estimate_branch_context(&before).tokens;
        drop(log_guard);

        bus.emit(AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason: aj_agent::events::CompactionReason::Threshold,
            tokens_before: 100,
            tokens_after: 0,
            usage: None,
            summary: None,
            error: Some("failed after checkpoint".into()),
        })
        .await
        .unwrap();
        let guard = log.lock().await;
        let resumed = SessionLog::resume(
            &ProjectSessions::new(guard.path().parent().unwrap().to_path_buf()),
            guard.session_id(),
        )
        .unwrap();
        let after = resumed.linearize(resumed.head().unwrap(), ThreadFilter::MAIN);
        assert_eq!(serde_json::to_value(after.messages()).unwrap(), messages);
        assert_eq!(
            crate::compaction::estimate_branch_context(&after).tokens,
            occupancy
        );
        assert_eq!(resumed.stats().unwrap().compactions, 1);
        assert!(
            crate::compaction::prepare_compaction(&after, 1).is_none(),
            "a transcript error must not make a checkpoint eligible for recompaction"
        );
    }

    #[tokio::test]
    async fn compaction_failure_survives_reopen_without_changing_context_or_accounting() {
        use aj_agent::events::CompactionReason;

        for agent_id in [AgentId::Main, AgentId::Sub(1)] {
            let (_dir, log) = fresh_log();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let bus = EventBus::new();
            let journal = Journal::new(Arc::clone(&log));
            journal.stream_into(tx);
            bus.set_committer(journal.committer());
            bus.emit(AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: user_msg("hi"),
            })
            .await
            .unwrap();
            if agent_id != AgentId::Main {
                bus.emit(sub_start(1, "do thing")).await.unwrap();
            }
            bus.emit(AgentEvent::MessageEnd {
                agent_id,
                message: assistant_text("answer"),
            })
            .await
            .unwrap();
            let filter = match agent_id {
                AgentId::Main => ThreadFilter::MAIN,
                AgentId::Sub(n) => ThreadFilter::sub(n),
            };
            let guard = log.lock().await;
            let before = guard.linearize(&guard.latest_leaf(filter).unwrap(), filter);
            assert_eq!(
                before.message_count(),
                if agent_id == AgentId::Main { 2 } else { 1 }
            );
            let messages = serde_json::to_value(before.agent_messages()).unwrap();
            let occupancy = crate::compaction::estimate_branch_context(&before).tokens;
            let stats = guard.stats().unwrap();
            let persistence = ProjectSessions::new(guard.path().parent().unwrap().to_path_buf());
            let session_id = guard.session_id().to_string();
            let count = guard.len();
            drop(guard);
            drained(&mut rx);

            let failure = AgentEvent::CompactionEnd {
                agent_id,
                reason: CompactionReason::Overflow,
                tokens_before: 1234,
                tokens_after: 0,
                usage: None,
                summary: None,
                error: Some("summarizer unavailable".into()),
            };
            bus.emit(failure.clone()).await.unwrap();
            let forwarded = drained(&mut rx);
            assert_eq!(forwarded.len(), 1);
            let identity = forwarded[0].entry.clone().expect("failure is durable");
            assert_eq!(identity.seq, u64::try_from(count).unwrap() + 1);

            bus.emit(AgentEvent::CompactionEnd {
                agent_id,
                reason: CompactionReason::Manual,
                tokens_before: 1234,
                tokens_after: 0,
                usage: None,
                summary: None,
                error: None,
            })
            .await
            .unwrap();
            let canceled = drained(&mut rx);
            assert_eq!(canceled.len(), 1);
            assert!(canceled[0].entry.is_none());
            assert_eq!(log.lock().await.len(), count + 1);
            drop(journal);
            drop(bus);
            drop(log);

            // Reopen: the failure entry was committed as it was appended.
            let resumed = SessionLog::resume(&persistence, &session_id).unwrap();
            assert_eq!(resumed.len(), count + 1);
            let after = resumed.linearize(&resumed.latest_leaf(filter).unwrap(), filter);
            assert_eq!(
                serde_json::to_value(after.agent_messages()).unwrap(),
                messages
            );
            assert_eq!(
                crate::compaction::estimate_branch_context(&after).tokens,
                occupancy
            );
            assert_eq!(
                serde_json::to_value(resumed.stats().unwrap().usage).unwrap(),
                serde_json::to_value(stats.usage).unwrap()
            );
            assert_eq!(
                serde_json::to_value(resumed.stats().unwrap().compaction_usage).unwrap(),
                serde_json::to_value(stats.compaction_usage).unwrap()
            );
            assert_eq!(resumed.stats().unwrap().compactions, stats.compactions);
            let snapshot = resumed.snapshot();
            let replayed: Vec<_> =
                project_suffix(&snapshot, Some(identity.seq - 1), &BTreeSet::new())
                    .filter(|tagged| matches!(tagged.event, AgentEvent::CompactionEnd { .. }))
                    .collect();
            assert_eq!(replayed.len(), 1, "cancellation must not replay");
            assert_eq!(replayed[0].entry, Some(identity));
            assert_eq!(
                serde_json::to_value(&replayed[0].event).unwrap(),
                serde_json::to_value(failure).unwrap()
            );
        }
    }

    /// An entry no event stands for still reaches the stream, as a
    /// position, in order with the events around it. A transient event the
    /// write returns stays untagged.
    #[tokio::test]
    async fn record_publishes_an_entry_without_an_event_as_a_position() {
        let (_dir, log) = fresh_log();
        let journal = Journal::new(Arc::clone(&log));
        let mut rx = journal.stream();
        let user = |text| AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg(text),
        };
        journal.commit(&user("first")).await.expect("commit");

        let entry = journal
            .record(|log| {
                let entry = log.append_thinking_change(ThreadFilter::MAIN, "high")?;
                let confirmation = TaggedEvent {
                    entry: None,
                    event: AgentEvent::Notice {
                        agent_id: AgentId::Main,
                        text: "confirmed".into(),
                    },
                    branch_settings: None,
                };
                Ok((entry, vec![confirmation]))
            })
            .await
            .expect("record");
        journal
            .record(|_| Ok(((), Vec::new())))
            .await
            .expect("a write that appends nothing");
        journal.commit(&user("second")).await.expect("commit");

        let mut seen = Vec::new();
        while let Ok(item) = rx.try_recv() {
            seen.push(match item {
                Committed::Event(tagged) => ("event", tagged.entry.map(|entry| entry.seq)),
                Committed::Position(seq) => ("position", Some(seq)),
            });
        }
        assert_eq!(entry.seq, 3);
        assert_eq!(
            seen,
            vec![
                ("event", Some(2)),
                ("event", None),
                ("position", Some(3)),
                ("event", Some(4)),
            ]
        );

        journal.close();
        let refused = journal.record(|log| {
            let entry = log.append_thinking_change(ThreadFilter::MAIN, "off")?;
            Ok((entry, Vec::new()))
        });
        assert!(matches!(
            refused.await,
            Err(crate::log::SessionError::Closed)
        ));
        assert_eq!(
            log.lock().await.last_seq(),
            4,
            "a refused write appends nothing"
        );
    }

    /// A tool batch persists one entry per result, so the journal has to
    /// tag each result's `MessageEnd` with its own entry. Tagging from the
    /// log's length at delivery time would give them all the same
    /// position.
    #[tokio::test]
    async fn forwarder_tags_each_result_of_a_tool_batch_with_its_own_entry() {
        let (_dir, log) = fresh_log();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = EventBus::new();
        let journal = Journal::new(Arc::clone(&log));
        journal.stream_into(tx);
        bus.set_committer(journal.committer());

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("read three files"),
        })
        .await
        .expect("emit the prompt");
        let batch = AgentMessage::wire(Message::Assistant(AssistantMessage {
            content: (1..=3)
                .map(|n| {
                    AssistantContent::ToolCall(aj_models::types::ToolCall {
                        is_raw: false,
                        id: format!("tu-{n}"),
                        name: "read_file".into(),
                        arguments: serde_json::json!({"path": format!("/tmp/{n}")}),
                    })
                })
                .collect(),
            ..AssistantMessage::empty()
        }));
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: batch,
        })
        .await
        .expect("emit the batch");
        let results: Vec<AgentMessage> = (1..=3)
            .map(|n| tool_result(&format!("tu-{n}"), "read_file", &format!("body {n}")))
            .collect();
        for result in &results {
            bus.emit(AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: result.clone(),
            })
            .await
            .expect("emit a tool result");
        }

        let forwarded = drained(&mut rx);
        // Entry 1 is the system prompt: the prompt, the batch and its
        // three results follow.
        assert_eq!(durable_seqs(&forwarded), vec![2, 3, 4, 5, 6]);
        for tagged in &forwarded {
            let entry = tagged.entry.as_ref().expect("every MessageEnd is durable");
            assert_eq!(
                Some(entry.id.clone()),
                entry_id_at(&log, entry.seq).await,
                "tag names its own entry"
            );
        }
        let tagged_results: Vec<&str> = forwarded[2..]
            .iter()
            .map(|tagged| tagged.entry.as_ref().expect("durable").id.as_str())
            .collect();
        let expected: Vec<&str> = results.iter().map(|message| message.id()).collect();
        assert_eq!(
            tagged_results, expected,
            "each result is tagged with its own message id"
        );
    }

    #[tokio::test]
    async fn forwarder_survives_a_dropped_receiver() {
        let (_dir, log) = fresh_log();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = EventBus::new();
        let journal = Journal::new(Arc::clone(&log));
        journal.stream_into(tx);
        bus.set_committer(journal.committer());
        drop(rx);

        // The bus awaits listeners inline and a listener error is a fatal
        // turn error, so an absent consumer must not surface as one.
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("hi"),
        })
        .await
        .expect("emit must not fail on a closed sink");
        assert_eq!(
            log.lock().await.len(),
            2,
            "the message is still persisted with no consumer attached"
        );
    }
}
