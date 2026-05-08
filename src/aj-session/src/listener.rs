//! Bus listener that drives the conversation log off of
//! [`AgentEvent::MessagePersisted`].
//!
//! Per `docs/aj-next-plan.md` §2.3b and §2.4b, the agent emits a
//! typed [`AgentEvent::MessagePersisted`] for every payload that
//! needs to hit disk: the user's typed prompt, the assistant
//! message, the synthesized user-role message carrying tool-result
//! content blocks, and the freestanding [`UserOutput`] entries
//! written for tool-call parse failures and tool-execution errors.
//! A persistence listener subscribed to the agent's bus owns the
//! [`ConversationLog`] handle and translates each event into one
//! [`ConversationView`] append.
//!
//! Because the bus awaits each listener inline (`docs/aj-next-plan.md`
//! §1.4 — "Hook vs subscriber pattern"), the listener returning
//! `Err` aborts the run with a fatal turn error — preserving the
//! same durability guarantee the previous direct `view.add_*` calls
//! had.
//!
//! Sub-agent first-entry anchoring (per `docs/aj-next-plan.md` §1.6)
//! is the listener's responsibility too: when the agent emits
//! [`AgentEvent::SubAgentStart`] the listener captures the parent
//! thread's current head; the next [`AgentEvent::MessagePersisted`]
//! tagged with the spawned `Sub(n)` agent id (whose own thread is
//! still empty) anchors at that captured head. Subsequent
//! sub-agent writes follow the chain via
//! [`ConversationLog::latest_leaf`] like any other thread.
//!
//! The agent (after §2.4b) never reaches into the log directly, so
//! the listener has exclusive write access; the binary takes brief
//! read locks to resolve the system prompt, snapshot the thread
//! for replay, and display the final usage summary.

use std::collections::HashMap;
use std::sync::Arc;

use aj_agent::bus::Listener;
use aj_agent::events::{AgentEvent, AgentId, PersistedMessageKind};
use anyhow::{Result, anyhow};
use tokio::sync::Mutex as TokioMutex;

use crate::log::{ConversationLog, ConversationView, EntryId, ThreadFilter};

/// Build a [`Listener`] that writes every
/// [`AgentEvent::MessagePersisted`] to the given log handle.
///
/// Other event variants are intentional no-ops here, with one
/// exception: [`AgentEvent::SubAgentStart`] populates an internal
/// "first-entry anchor" map so the freshly-spawned sub-agent's
/// initial write can be threaded under the parent's current head.
/// Without this hook the sub-agent's first write would have no
/// reachable parent (its own [`ThreadFilter::subagent`] thread is
/// empty), and the listener would error out.
pub fn persistence_listener(log: Arc<TokioMutex<ConversationLog>>) -> Listener {
    // `Sub(n)` ↦ parent's `latest_leaf` at spawn time. Populated by
    // [`AgentEvent::SubAgentStart`], drained by the first
    // [`AgentEvent::MessagePersisted`] tagged with `Sub(n)`. A
    // tokio mutex keeps the access points async-friendly even
    // though the map itself is touched only briefly.
    let pending_anchors: Arc<TokioMutex<HashMap<usize, EntryId>>> =
        Arc::new(TokioMutex::new(HashMap::new()));

    Arc::new(move |event: &AgentEvent| {
        let log = Arc::clone(&log);
        let anchors = Arc::clone(&pending_anchors);
        // Capture the event by value so the returned future doesn't
        // borrow from the bus's snapshot. `AgentEvent` is `Clone`
        // (cheap field clones) so this is fine for our event volume.
        let event = event.clone();
        Box::pin(async move {
            match event {
                AgentEvent::SubAgentStart { parent, child, .. } => {
                    let AgentId::Sub(child_n) = child else {
                        // The agent only ever spawns sub-agents
                        // (`Main` is reserved for the top-level
                        // instance), so this branch should never
                        // fire; treating it as an error guards the
                        // invariant.
                        return Err(anyhow!("SubAgentStart with non-Sub child {child:?}"));
                    };
                    let parent_filter = filter_for(parent);
                    let log_guard = log.lock().await;
                    let parent_head = log_guard.latest_leaf(parent_filter).ok_or_else(|| {
                        anyhow!(
                            "SubAgentStart: parent {parent:?} thread has no head entry to anchor child {child:?} at"
                        )
                    })?;
                    drop(log_guard);
                    anchors.lock().await.insert(child_n, parent_head);
                }
                AgentEvent::MessagePersisted { agent_id, kind } => {
                    let mut log_guard = log.lock().await;
                    let mut anchors_guard = anchors.lock().await;
                    persist(&mut log_guard, &mut anchors_guard, agent_id, kind)?;
                }
                _ => {}
            }
            Ok(())
        })
    })
}

/// Append one entry to the log on behalf of `agent_id`.
///
/// For [`AgentId::Main`] the parent for the new entry is the user
/// thread's current `latest_leaf` (or `None`, anchoring at the
/// system-prompt root for fresh threads). For [`AgentId::Sub(n)`]
/// it's the sub-agent thread's own `latest_leaf` once that thread
/// has at least one entry, falling back to the captured parent
/// anchor from [`AgentEvent::SubAgentStart`] for the very first
/// write.
fn persist(
    log: &mut ConversationLog,
    pending_anchors: &mut HashMap<usize, EntryId>,
    agent_id: AgentId,
    kind: PersistedMessageKind,
) -> Result<()> {
    let mut view = match agent_id {
        AgentId::Main => {
            // `latest_leaf` returning `None` is fine here: the user
            // thread can be empty on a fresh log (only the
            // system-prompt root entry exists yet).
            // [`ConversationView::user`] anchors at that root
            // automatically.
            let head = log.latest_leaf(ThreadFilter::USER);
            ConversationView::user(log, head)
        }
        AgentId::Sub(n) => {
            // Sub-agent thread already has an entry: chain on its
            // current leaf. Otherwise drain the pending anchor
            // captured from [`AgentEvent::SubAgentStart`].
            let head = log
                .latest_leaf(ThreadFilter::subagent(n))
                .or_else(|| pending_anchors.remove(&n))
                .ok_or_else(|| {
                    anyhow!(
                        "persistence listener: sub-agent {n} thread has no head entry and no parent anchor was captured"
                    )
                })?;
            ConversationView::subagent(log, head, n)
        }
    };

    match kind {
        PersistedMessageKind::User { content } => {
            view.add_user_message(content)?;
        }
        PersistedMessageKind::Assistant { content } => {
            view.add_assistant_message(content)?;
        }
        PersistedMessageKind::ToolResult { content } => {
            view.add_user_message(content)?;
        }
        PersistedMessageKind::UserOutput { output } => {
            view.add_user_output(output)?;
        }
    }
    Ok(())
}

/// Translate an [`AgentId`] into the [`ThreadFilter`] selecting that
/// agent's own thread.
fn filter_for(agent_id: AgentId) -> ThreadFilter {
    match agent_id {
        AgentId::Main => ThreadFilter::USER,
        AgentId::Sub(n) => ThreadFilter::subagent(n),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aj_agent::bus::EventBus;
    use aj_agent::events::{AgentEvent, AgentId, PersistedMessageKind};
    use aj_agent::types::UserOutput;
    use aj_models::messages::{ContentBlockParam, Role};
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    use super::persistence_listener;
    use crate::log::{ConversationEntryKind, ConversationLog, ConversationView, ThreadFilter};
    use crate::persistence::ConversationPersistence;

    /// Set up a temp threads dir + a fresh log with a frozen system
    /// prompt root.
    fn fresh_log() -> (TempDir, Arc<TokioMutex<ConversationLog>>) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("threads"));
        let mut log = ConversationLog::create(&persistence).expect("create log");
        log.set_system_prompt("test system prompt".to_string())
            .expect("set system prompt");
        (dir, Arc::new(TokioMutex::new(log)))
    }

    #[tokio::test]
    async fn user_kind_appends_to_empty_user_thread() {
        // First user message on a fresh thread anchors at the
        // SystemPrompt root entry.
        let (_dir, log) = fresh_log();
        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        bus.emit(AgentEvent::MessagePersisted {
            agent_id: AgentId::Main,
            kind: PersistedMessageKind::User {
                content: vec![ContentBlockParam::new_text_block("hi".into())],
            },
        })
        .await
        .expect("emit");

        let log_guard = log.lock().await;
        let head = log_guard
            .latest_leaf(ThreadFilter::USER)
            .expect("user-thread head exists after emit");
        let convo = log_guard.linearize(&head, ThreadFilter::USER);
        let last = convo
            .last_message()
            .expect("conversation has at least one message");
        assert!(matches!(last.role, Role::User));
        assert_eq!(last.content.len(), 1);
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
    async fn sub_agent_first_entry_anchors_at_parent_head() {
        // The persistence listener must capture the parent's
        // `latest_leaf` from `SubAgentStart` and use it as the
        // anchor for the sub-agent's first `MessagePersisted`
        // event. Subsequent writes follow the sub-agent thread's
        // own leaf.
        let (_dir, log) = fresh_log();
        // Seed an assistant message on the parent thread to act as
        // the spawning anchor — that's where the parent's
        // `tool_use` block would live in production.
        let parent_anchor = {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard, None);
            view.add_user_message(vec![ContentBlockParam::new_text_block("hi".into())])
                .expect("user msg");
            view.add_assistant_message(vec![ContentBlockParam::new_text_block("ack".into())])
                .expect("assistant msg");
            log_guard
                .latest_leaf(ThreadFilter::USER)
                .expect("parent anchor exists")
        };

        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        // Spawn announcement → captures parent_anchor for Sub(1).
        bus.emit(AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(1),
            task: "do thing".into(),
        })
        .await
        .expect("emit start");

        // Sub-agent's first write is its user-prompt message.
        bus.emit(AgentEvent::MessagePersisted {
            agent_id: AgentId::Sub(1),
            kind: PersistedMessageKind::User {
                content: vec![ContentBlockParam::new_text_block("do it".into())],
            },
        })
        .await
        .expect("emit user");

        // Sub-agent's second write is its assistant reply; this
        // one should chain off the freshly-appended sub-agent leaf
        // (no parent_anchor lookup needed).
        bus.emit(AgentEvent::MessagePersisted {
            agent_id: AgentId::Sub(1),
            kind: PersistedMessageKind::Assistant {
                content: vec![ContentBlockParam::new_text_block("done".into())],
            },
        })
        .await
        .expect("emit assistant");

        let log_guard = log.lock().await;
        let sub_head = log_guard
            .latest_leaf(ThreadFilter::subagent(1))
            .expect("sub-agent thread head exists");
        let convo = log_guard.linearize(&sub_head, ThreadFilter::subagent(1));
        // 2 messages: User + Assistant, in source order.
        let entries: Vec<_> = convo.entries().to_vec();
        assert_eq!(entries.len(), 2, "got entries: {entries:#?}");
        // The first sub-agent entry must be parented at the
        // parent's anchor.
        let first = &entries[0];
        assert_eq!(first.parent_id.as_ref(), Some(&parent_anchor));
    }

    #[tokio::test]
    async fn sub_agent_assistant_without_anchor_returns_error() {
        // The listener treats a missing thread head as a hard error
        // for sub-agent writes when no `SubAgentStart` was observed
        // beforehand: there's no captured anchor and the sub-agent
        // thread has no leaf yet. The bus surfaces the error back
        // to the caller of `emit`.
        let (_dir, log) = fresh_log();
        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        let err = bus
            .emit(AgentEvent::MessagePersisted {
                agent_id: AgentId::Sub(2),
                kind: PersistedMessageKind::Assistant {
                    content: Vec::new(),
                },
            })
            .await
            .expect_err("emit should fail when sub-agent thread is empty");
        assert!(err.to_string().contains("no head entry"));
    }

    #[tokio::test]
    async fn non_persistence_events_do_nothing() {
        // Other event variants (notices, warnings, lifecycle markers)
        // flow through the listener as no-ops. The log stays at
        // exactly one entry (the system prompt).
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
