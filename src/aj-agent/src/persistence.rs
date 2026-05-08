//! Bus listener that drives the conversation log off of
//! [`AgentEvent::MessagePersisted`].
//!
//! Per `docs/aj-next-plan.md` §2.3b, the agent emits a typed
//! [`AgentEvent::MessagePersisted`] for every payload that needs to
//! hit disk (the assistant message, the synthesized tool-result user
//! message, and the freestanding [`UserOutput`] entries written for
//! tool-call parse failures and tool-execution errors). A persistence
//! listener subscribed to the agent's bus owns the
//! [`ConversationLog`] handle and translates each event into one
//! [`ConversationView`] append. Because the bus awaits each listener
//! inline (`docs/aj-next-plan.md` §1.4 — "Hook vs subscriber pattern"),
//! the listener returning `Err` aborts the run with
//! [`crate::TurnError::Fatal`], preserving the same durability
//! guarantee the previous direct `view.add_*` calls had.
//!
//! The agent and the listener share one
//! `Arc<tokio::sync::Mutex<ConversationLog>>`. The agent only locks
//! it briefly for reads (linearization for inference, `latest_leaf`
//! after emit to recover the freshly-appended entry's
//! [`EntryId`](aj_session::EntryId)) and never holds the lock across a
//! `bus.emit(...)` — that would deadlock against the listener's own
//! `lock().await`. The listener locks it long enough to write one
//! line and drops the guard.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::Mutex as TokioMutex;

use aj_session::{ConversationLog, ConversationView, ThreadFilter, ThreadKind};

use crate::bus::Listener;
use crate::events::{AgentEvent, AgentId, PersistedMessageKind};

/// Build a [`Listener`] that writes every
/// [`AgentEvent::MessagePersisted`] to the given log handle.
///
/// Other event variants are intentional no-ops here: rendering,
/// streaming, and lifecycle events flow through other listeners
/// (today the legacy `EventBridgeListener` in the `aj` binary, and
/// soon the TUI's event pump). The persistence listener stays
/// stateless beyond the shared [`ConversationLog`] handle so a
/// single registration covers the whole session, including any
/// sub-agent activity that arrives on the parent's bus tagged with
/// [`AgentId::Sub`].
pub fn persistence_listener(log: Arc<TokioMutex<ConversationLog>>) -> Listener {
    Arc::new(move |event: &AgentEvent| {
        let log = Arc::clone(&log);
        // Capture the event by value so the returned future doesn't
        // borrow from the bus's snapshot. `AgentEvent` is `Clone`
        // (cheap field clones — `Vec<ContentBlockParam>` and
        // `UserOutput` are small) so this is fine for our event
        // volume.
        let event = event.clone();
        Box::pin(async move {
            if let AgentEvent::MessagePersisted { agent_id, kind } = event {
                let mut log = log.lock().await;
                persist(&mut log, agent_id, kind)?;
            }
            Ok(())
        })
    })
}

/// Append one entry to the log, anchored at the latest leaf of the
/// thread the event came from.
///
/// `agent_id` selects the thread: [`AgentId::Main`] writes to the
/// user thread, [`AgentId::Sub(n)`] writes to the n-th sub-agent
/// thread. The append always uses the current `latest_leaf` of that
/// thread as the parent — the agent emits these events in turn
/// order so the chain stays well-formed without any extra plumbing.
fn persist(log: &mut ConversationLog, agent_id: AgentId, kind: PersistedMessageKind) -> Result<()> {
    let (thread, sub_id, filter) = match agent_id {
        AgentId::Main => (ThreadKind::User, None, ThreadFilter::USER),
        AgentId::Sub(n) => (ThreadKind::Subagent, Some(n), ThreadFilter::subagent(n)),
    };
    let head = log
        .latest_leaf(filter)
        .ok_or_else(|| anyhow!("persistence listener: thread {agent_id:?} has no head entry"))?;

    let mut view = match thread {
        ThreadKind::User => ConversationView::user(log, Some(head)),
        ThreadKind::Subagent => {
            // `sub_id` is `Some` whenever `thread == Subagent` per the
            // match above; the `expect` documents the invariant rather
            // than guarding a runtime check.
            ConversationView::subagent(log, head, sub_id.expect("subagent id is Some"))
        }
        ThreadKind::Meta => {
            // `Meta` is reserved for the [`ThreadKind::Meta`]
            // [`SystemPrompt`] root entry, which the agent writes
            // through [`ConversationLog::set_system_prompt`] up
            // front, never via [`AgentEvent::MessagePersisted`].
            return Err(anyhow!(
                "persistence listener: refusing to write to ThreadKind::Meta via the event bus"
            ));
        }
    };

    match kind {
        PersistedMessageKind::Assistant { content } => {
            view.add_assistant_message(content)?;
        }
        PersistedMessageKind::ToolResult { content } => {
            view.add_user_message(content)?;
        }
        PersistedMessageKind::UserOutput { output } => {
            view.add_user_output(output)?;
        }
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_models::messages::{ContentBlockParam, Role};
    use aj_session::{
        ConversationEntryKind, ConversationLog, ConversationPersistence, ConversationView,
        ThreadFilter,
    };
    use aj_ui::UserOutput;
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    use super::persistence_listener;
    use crate::bus::EventBus;
    use crate::events::{AgentEvent, AgentId, PersistedMessageKind};

    /// Set up a temp threads dir + a fresh log with a frozen system
    /// prompt root. Mirrors the helper in `event_protocol_tests` so
    /// each persistence test owns its own scratch directory.
    fn fresh_log() -> (TempDir, Arc<TokioMutex<ConversationLog>>) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("threads"));
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("test system prompt".to_string())
            .expect("set system prompt");
        (dir, Arc::new(TokioMutex::new(log)))
    }

    #[tokio::test]
    async fn assistant_kind_appends_user_thread_assistant_message() {
        let (_dir, log) = fresh_log();
        // Seed a user message so the assistant message has a parent
        // in the user thread; without this the listener would error
        // ("thread has no head entry") because the system-prompt
        // root is a Meta entry, not a User-thread one.
        {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("user msg");
        }

        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        let assistant_content = vec![ContentBlockParam::new_text_block("hello".into())];
        bus.emit(AgentEvent::MessagePersisted {
            agent_id: AgentId::Main,
            kind: PersistedMessageKind::Assistant {
                content: assistant_content.clone(),
            },
        })
        .await
        .expect("emit");

        let log_guard = log.lock().await;
        let head = log_guard
            .latest_leaf(ThreadFilter::USER)
            .expect("user-thread head exists");
        let convo = log_guard.linearize(&head, ThreadFilter::USER);
        let last = convo
            .last_message()
            .expect("conversation has at least one message");
        assert!(matches!(last.role, Role::Assistant));
        assert_eq!(last.content.len(), assistant_content.len());
    }

    #[tokio::test]
    async fn user_output_kind_appends_freestanding_entry() {
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("user msg");
        }

        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        bus.emit(AgentEvent::MessagePersisted {
            agent_id: AgentId::Main,
            kind: PersistedMessageKind::UserOutput {
                output: UserOutput::ToolError {
                    tool_name: "ping".into(),
                    input: "{}".into(),
                    error: "boom".into(),
                },
            },
        })
        .await
        .expect("emit");

        // The new entry shows up as a freestanding `UserOutput`
        // entry on the user thread (matching the legacy on-disk
        // shape; see the §2.0 reconnaissance findings in
        // `docs/aj-next-progress.md`).
        let log_guard = log.lock().await;
        let head = log_guard
            .latest_leaf(ThreadFilter::USER)
            .expect("latest user-thread leaf exists");
        let entries: Vec<_> = log_guard.entries_in_order().into_iter().cloned().collect();
        let last = entries.last().expect("log has entries");
        assert_eq!(&last.id, &head);
        assert!(matches!(
            last.entry,
            ConversationEntryKind::UserOutput(UserOutput::ToolError { .. })
        ));
    }

    #[tokio::test]
    async fn empty_thread_returns_error_instead_of_panicking() {
        // The listener treats a missing thread head as a hard error
        // (the agent should never emit MessagePersisted before at
        // least one entry exists on the target thread); the bus
        // surfaces it back to the caller of `emit`.
        let (_dir, log) = fresh_log();
        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        let err = bus
            .emit(AgentEvent::MessagePersisted {
                agent_id: AgentId::Main,
                kind: PersistedMessageKind::Assistant {
                    content: Vec::new(),
                },
            })
            .await
            .expect_err("emit should fail when the thread is empty");
        assert!(err.to_string().contains("no head entry"));
    }

    #[tokio::test]
    async fn non_persistence_events_do_nothing() {
        // Other event variants flow through the listener as no-ops.
        // The log stays at exactly one entry (the system prompt).
        let (_dir, log) = fresh_log();
        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        bus.emit(AgentEvent::Notice {
            agent_id: AgentId::Main,
            text: "ignored".into(),
        })
        .await
        .expect("emit");

        let log_guard = log.lock().await;
        // Only the system prompt root is present.
        assert_eq!(log_guard.len(), 1);
    }
}
