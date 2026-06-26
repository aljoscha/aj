//! Bus listener that drives the conversation log off of
//! [`AgentEvent::MessageEnd`].
//!
//! The agent emits a typed
//! [`AgentEvent::MessageEnd`] for every payload that needs to hit
//! disk: the user's typed prompt, the assistant message at the end
//! of every inference, and one tool_result message per tool call in
//! a tool batch. A persistence listener subscribed to the agent's
//! bus owns the [`ConversationLog`] handle and translates each
//! [`MessageEnd`] event into one `ConversationView::add_message`
//! call.
//!
//! Because the bus awaits each listener inline, the listener
//! returning `Err` aborts the run with a fatal turn error, so a
//! failed write stops the turn rather than silently losing a message.
//!
//! Sub-agent first-entry anchoring is the listener's responsibility
//! too: when the agent emits
//! [`AgentEvent::SubAgentStart`] the listener captures the parent
//! thread's current head and immediately writes the sub-agent's
//! [`crate::log::ConversationEntryKind::SubAgentSpawn`] root entry
//! (carrying the task and the settings snapshot from the event)
//! anchored at that head. The sub-agent's first
//! [`AgentEvent::MessageEnd`] then chains onto the spawn entry via
//! [`ConversationLog::latest_leaf`], like every subsequent
//! sub-agent write. A `Sub(n)` message arriving with no prior
//! `SubAgentStart` (and hence an empty sub thread) is an error.
//!
//! Write ownership is split with the binary: the listener has
//! exclusive ownership of *message* writes and of sub-agent spawn
//! entries (spawns happen inside the agent); main-thread settings
//! entries are appended by the binary, which already holds the log
//! handle and owns the run-config state they record. The binary
//! additionally takes brief read locks to resolve the system
//! prompt, snapshot the thread for replay, and display the final
//! usage summary.

use std::sync::Arc;

use aj_agent::BoxError;
use aj_agent::bus::Listener;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::message::AgentMessage;
use tokio::sync::Mutex as TokioMutex;

use crate::log::{ConversationLog, ConversationView, ThreadFilter};

/// Build a [`Listener`] that writes every finalized
/// [`AgentEvent::MessageEnd`] to the given log handle.
///
/// Other event variants are intentional no-ops here, with one
/// exception: [`AgentEvent::SubAgentStart`] writes the spawned
/// sub-agent's `SubAgentSpawn` root entry, anchored at the parent
/// thread's current head. Without this hook the sub-agent's first
/// write would have no reachable parent (its own
/// [`ThreadFilter::subagent`] thread is empty), and the listener
/// would error out.
pub fn persistence_listener(log: Arc<TokioMutex<ConversationLog>>) -> Listener {
    Arc::new(move |event: &AgentEvent| {
        let log = Arc::clone(&log);
        let event = event.clone();
        Box::pin(async move {
            match event {
                AgentEvent::SubAgentStart {
                    parent,
                    child,
                    task,
                    settings,
                } => {
                    let AgentId::Sub(child_n) = child else {
                        return Err(format!("SubAgentStart with non-Sub child {child:?}").into());
                    };
                    let parent_filter = filter_for(parent);
                    let mut log_guard = log.lock().await;
                    let parent_head = log_guard.latest_leaf(parent_filter).ok_or_else(|| {
                        BoxError::from(format!(
                            "SubAgentStart: parent {parent:?} thread has no head entry to anchor child {child:?} at"
                        ))
                    })?;
                    log_guard.append_subagent_spawn(child_n, parent_head, &task, &settings)?;
                }
                AgentEvent::MessageEnd { agent_id, message } => {
                    let mut log_guard = log.lock().await;
                    persist(&mut log_guard, agent_id, message)?;
                }
                _ => {}
            }
            Ok(())
        })
    })
}

/// Append one finalized message to the log on behalf of `agent_id`.
///
/// For [`AgentId::Main`] the parent for the new entry is the user
/// thread's current `latest_leaf` (or `None`, anchoring at the
/// system-prompt root for fresh threads). For [`AgentId::Sub(n)`]
/// it's the sub-agent thread's own `latest_leaf`; the thread is
/// never empty for a legitimately spawned sub-agent because
/// [`AgentEvent::SubAgentStart`] seeds it with a `SubAgentSpawn`
/// entry.
fn persist(
    log: &mut ConversationLog,
    agent_id: AgentId,
    message: AgentMessage,
) -> Result<(), BoxError> {
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
            let head = log
                .latest_leaf(ThreadFilter::subagent(n))
                .ok_or_else(|| {
                    BoxError::from(format!(
                        "persistence listener: sub-agent {n} thread has no head entry; was SubAgentStart emitted?"
                    ))
                })?;
            ConversationView::subagent(log, head, n)
        }
    };

    view.add_message(message)?;
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
    use aj_agent::events::{AgentEvent, AgentId, AgentSettings};
    use aj_agent::message::AgentMessage;
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, TextContent, ToolResultMessage, UserMessage,
    };
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    use super::persistence_listener;
    use crate::log::{
        ConversationEntry, ConversationEntryKind, ConversationLog, ConversationView, ThreadFilter,
    };
    use crate::persistence::ConversationPersistence;

    /// Set up a temp sessions dir + a fresh log with a frozen system
    /// prompt root.
    fn fresh_log() -> (TempDir, Arc<TokioMutex<ConversationLog>>) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("sessions"));
        let mut log = ConversationLog::create(&persistence).expect("create log");
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

    /// A SubAgentStart event carrying a representative bundle
    /// identity.
    fn sub_start(n: usize, task: &str) -> AgentEvent {
        AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(n),
            task: task.to_string(),
            settings: AgentSettings {
                provider: "anthropic".to_string(),
                model_id: "claude-x".to_string(),
                thinking: "medium".to_string(),
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
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("hi"),
        })
        .await
        .expect("emit");

        let log_guard = log.lock().await;
        let head = log_guard
            .latest_leaf(ThreadFilter::USER)
            .expect("user-thread head exists after emit");
        let convo = log_guard.linearize(&head, ThreadFilter::USER);
        let last = convo.last_message().expect("at least one message");
        assert!(matches!(last, Message::User(_)));
    }

    #[tokio::test]
    async fn assistant_message_appends_to_user_thread() {
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard, None);
            view.add_message(user_msg("hi")).expect("user msg");
        }

        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: assistant_text("hello"),
        })
        .await
        .expect("emit");

        let log_guard = log.lock().await;
        let head = log_guard
            .latest_leaf(ThreadFilter::USER)
            .expect("user-thread head exists");
        let convo = log_guard.linearize(&head, ThreadFilter::USER);
        let last = convo.last_message().expect("at least one message");
        assert!(matches!(last, Message::Assistant(_)));
    }

    #[tokio::test]
    async fn tool_result_message_appends_to_user_thread() {
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard, None);
            view.add_message(user_msg("hi")).expect("u");
            // Assistant turn carrying a tool call.
            let assistant = AgentMessage::wire(Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::ToolCall(aj_models::types::ToolCall {
                    id: "tu-1".into(),
                    name: "ping".into(),
                    arguments: serde_json::json!({}),
                })],
                ..AssistantMessage::empty()
            }));
            view.add_message(assistant).expect("a");
        }

        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

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
            ConversationEntryKind::Message { message: m } => match m.as_wire() {
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
    async fn sub_agent_start_writes_spawn_entry_anchored_at_parent_head() {
        // `SubAgentStart` must immediately seed the sub thread with
        // one `SubAgentSpawn` entry anchored at the parent's
        // `latest_leaf`; the sub-agent's first `MessageEnd` then
        // chains onto it.
        let (_dir, log) = fresh_log();
        let parent_anchor = {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard, None);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_text("ack")).expect("a");
            log_guard
                .latest_leaf(ThreadFilter::USER)
                .expect("parent anchor exists")
        };

        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

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

        let log_guard = log.lock().await;
        let sub_head = log_guard
            .latest_leaf(ThreadFilter::subagent(1))
            .expect("sub-agent thread head exists");
        let convo = log_guard.linearize(&sub_head, ThreadFilter::subagent(1));
        let entries: Vec<_> = convo.entries().to_vec();
        // One spawn entry followed by the two messages.
        assert_eq!(entries.len(), 3, "got entries: {entries:#?}");
        match &entries[0].entry {
            ConversationEntryKind::SubAgentSpawn { task, settings } => {
                assert_eq!(task, "do thing");
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
        assert!(matches!(
            entries[1].entry,
            ConversationEntryKind::Message { .. }
        ));
        assert_eq!(entries[2].parent_id.as_ref(), Some(&entries[1].id));
    }

    #[tokio::test]
    async fn sub_agent_continuation_chains_on_existing_subthread() {
        // A re-prompt of a retained sub-agent emits no new
        // `SubAgentStart`; its `MessageEnd` events must chain onto the
        // existing sub-thread leaf, not re-anchor at the parent head.
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard, None);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_text("ack")).expect("a");
        }

        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

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
            .latest_leaf(ThreadFilter::subagent(1))
            .expect("sub-agent thread head exists");
        let convo = log_guard.linearize(&sub_head, ThreadFilter::subagent(1));
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
    fn entry_text(entry: &ConversationEntry) -> String {
        let message = match &entry.entry {
            ConversationEntryKind::Message { message } => message,
            other => panic!("expected Message entry, got {other:?}"),
        };
        match message.as_wire() {
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
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

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
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

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
}
