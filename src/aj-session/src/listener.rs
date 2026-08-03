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
use tokio::sync::mpsc::UnboundedSender;

use crate::log::{ConversationLog, ConversationView, EntryRef, ThreadFilter};

/// One event as it left the bus, paired with the log entry it appended.
///
/// `entry` is `Some` exactly for the durable events (spec 6.4): the
/// `MessageEnd` that persisted a message, the `SubAgentStart` that wrote
/// a spawn root, and the `CompactionEnd` of a compaction that recorded a
/// checkpoint.
#[derive(Debug, Clone)]
pub struct PersistedEvent {
    pub entry: Option<EntryRef>,
    pub event: AgentEvent,
}

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
            persist(&log, &event).await?;
            Ok(())
        })
    })
}

/// Build a [`Listener`] that persists exactly like
/// [`persistence_listener`] and forwards every event to `sink`, tagged
/// with the entry it appended.
///
/// The tag is taken at the append site. A consumer that instead read the
/// log's length when it received the event would race concurrent
/// sub-agent appends and mis-number the event (spec section 5).
///
/// The send is non-blocking and a closed receiver is ignored, so a slow
/// or absent consumer can never stall or fail a turn even though the bus
/// awaits this listener inline. That inline position is what gives the
/// sink its guarantee: an event it receives is already on disk.
pub fn persisting_forwarder(
    log: Arc<TokioMutex<ConversationLog>>,
    sink: UnboundedSender<PersistedEvent>,
) -> Listener {
    Arc::new(move |event: &AgentEvent| {
        let log = Arc::clone(&log);
        let sink = sink.clone();
        let event = event.clone();
        Box::pin(async move {
            let entry = match persist(&log, &event).await? {
                Some(entry) => Some(entry),
                None => compaction_entry(&log, &event).await,
            };
            let _ = sink.send(PersistedEvent { entry, event });
            Ok(())
        })
    })
}

/// Write whatever `event` persists and return the appended entry, or
/// `None` for an event that persists nothing. Takes the log lock only in
/// the arms that write, so the streaming events (by far the most
/// frequent) never contend for it.
async fn persist(
    log: &TokioMutex<ConversationLog>,
    event: &AgentEvent,
) -> Result<Option<EntryRef>, BoxError> {
    match event {
        AgentEvent::SubAgentStart {
            parent,
            child,
            task,
            background,
            settings,
        } => {
            let AgentId::Sub(child_n) = child else {
                return Err(format!("SubAgentStart with non-Sub child {child:?}").into());
            };
            let mut log_guard = log.lock().await;
            // Anchor the spawn root at the main thread's current
            // head. A sub-agent cannot spawn a sub-agent (the
            // `agent` tool is removed from its toolset), so the
            // parent is always the main thread.
            let parent_head = log_guard.head().cloned().ok_or_else(|| {
                BoxError::from(format!(
                    "SubAgentStart: parent {parent:?} thread has no head entry to anchor child {child:?} at"
                ))
            })?;
            let appended = log_guard.append_subagent_spawn(
                *child_n,
                parent_head,
                task,
                *background,
                settings,
            )?;
            Ok(Some(appended))
        }
        AgentEvent::MessageEnd { agent_id, message } => {
            let mut log_guard = log.lock().await;
            let appended = persist_message(&mut log_guard, *agent_id, message.clone())?;
            Ok(Some(appended))
        }
        _ => Ok(None),
    }
}

/// The `Compaction` entry a [`AgentEvent::CompactionEnd`] belongs to,
/// `None` for any other event.
///
/// The checkpoint is appended by the compaction run rather than by this
/// listener, so the entry is resolved by lookup instead of returned from
/// an append. Sound because at most one compaction runs per session at a
/// time and the bus awaits this listener inline, right after the append.
/// A failed or canceled compaction appends nothing and carries no
/// summary, which is what the summary match gates on.
async fn compaction_entry(
    log: &TokioMutex<ConversationLog>,
    event: &AgentEvent,
) -> Option<EntryRef> {
    let AgentEvent::CompactionEnd {
        agent_id,
        summary: Some(_),
        ..
    } = event
    else {
        return None;
    };
    let filter = match agent_id {
        AgentId::Main => ThreadFilter::USER,
        AgentId::Sub(n) => ThreadFilter::subagent(*n),
    };
    log.lock().await.latest_compaction(filter)
}

/// Append one finalized message to the log on behalf of `agent_id`.
///
/// For [`AgentId::Main`] the new entry anchors at the user thread's
/// explicit `head` (or the system-prompt root for a fresh thread). For
/// [`AgentId::Sub(n)`] it's the sub-agent thread's own `latest_leaf`;
/// the thread is never empty for a legitimately spawned sub-agent
/// because [`AgentEvent::SubAgentStart`] seeds it with a
/// `SubAgentSpawn` entry.
fn persist_message(
    log: &mut ConversationLog,
    agent_id: AgentId,
    message: AgentMessage,
) -> Result<EntryRef, BoxError> {
    let mut view = match agent_id {
        // A `None` head is fine here: the user thread can be empty on a
        // fresh log (only the system-prompt root exists yet), and
        // `ConversationView::user` anchors at that root automatically.
        AgentId::Main => ConversationView::user(log),
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

    Ok(view.add_message(message)?)
}

#[cfg(test)]
mod tests {
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

    use super::{PersistedEvent, persistence_listener, persisting_forwarder};
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
            background: false,
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
            let mut view = ConversationView::user(&mut log_guard);
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
            let mut view = ConversationView::user(&mut log_guard);
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
            ConversationEntryKind::Message { message: m } => match m.as_stored_wire() {
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
        let (_dir, log) = fresh_log();
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
        let _persist = bus.subscribe(persistence_listener(Arc::clone(&log)));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        let _capture = bus.subscribe(listener_from_sync(move |event| {
            captured_clone
                .lock()
                .expect("capture mutex")
                .push(event.clone());
        }));

        bus.emit(event).await.expect("emit tool result");

        let path = log.lock().await.path().to_path_buf();
        let raw = std::fs::read_to_string(path).expect("read JSONL");
        let tool_entry: serde_json::Value = raw
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSONL entry"))
            .find(|entry: &serde_json::Value| entry["message"]["role"] == "tool_result")
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
        let (_dir, log) = fresh_log();
        let parent_anchor = {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard);
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
            ConversationEntryKind::SubAgentSpawn { task, settings, .. } => {
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
    async fn sub_agent_end_appends_no_entry() {
        // Conclusions are not persisted. On resume the outcome is
        // reconstructed from the sub's final message stop reason, so the
        // listener must leave the sub thread's last entry as that message
        // and add nothing for `SubAgentEnd`.
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard);
            view.add_message(user_msg("hi")).expect("u");
            view.add_message(assistant_text("ack")).expect("a");
        }

        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));

        bus.emit(sub_start(1, "do thing"))
            .await
            .expect("emit start");
        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Sub(1),
            message: assistant_text("partial"),
        })
        .await
        .expect("emit assistant");

        let count_before = log.lock().await.entries_in_order().len();

        bus.emit(AgentEvent::SubAgentEnd {
            parent: AgentId::Main,
            child: AgentId::Sub(1),
            report: "sub-agent failed: boom".into(),
            conclusion: SubAgentConclusion::Failed,
        })
        .await
        .expect("emit end");

        let log_guard = log.lock().await;
        // `SubAgentEnd` adds no entry anywhere in the log.
        assert_eq!(
            log_guard.entries_in_order().len(),
            count_before,
            "SubAgentEnd must not append any entry"
        );
        let sub_head = log_guard
            .latest_leaf(ThreadFilter::subagent(1))
            .expect("sub-agent thread head exists");
        let convo = log_guard.linearize(&sub_head, ThreadFilter::subagent(1));
        let entries = convo.entries();
        // The sub thread's last entry is the sub's final assistant message,
        // not a conclusion marker.
        match &entries.last().expect("at least one entry").entry {
            ConversationEntryKind::Message { message } => {
                assert!(matches!(
                    message.as_stored_wire(),
                    Some(Message::Assistant(_))
                ));
            }
            other => panic!("expected the sub's final message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sub_agent_continuation_chains_on_existing_subthread() {
        // A re-prompt of a retained sub-agent emits no new
        // `SubAgentStart`; its `MessageEnd` events must chain onto the
        // existing sub-thread leaf, not re-anchor at the parent head.
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard);
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

    /// Drain everything the forwarder has already sent. The forwarder
    /// sends inline while the bus awaits it, so by the time an `emit`
    /// returns its event is in the channel.
    fn drained(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<PersistedEvent>,
    ) -> Vec<PersistedEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    /// Append position and id of every entry in the log, for checking
    /// what a tag names.
    async fn positions(log: &Arc<TokioMutex<ConversationLog>>) -> Vec<(u64, String)> {
        log.lock()
            .await
            .entries_in_order()
            .iter()
            .enumerate()
            .map(|(index, entry)| {
                (
                    u64::try_from(index).expect("fits u64") + 1,
                    entry.id.clone(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn forwarder_tags_the_durable_events_and_forwards_the_rest() {
        let (_dir, log) = fresh_log();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = EventBus::new();
        let _h = bus.subscribe(persisting_forwarder(Arc::clone(&log), tx));

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

        // A compaction checkpoint is written by the compaction run, not by
        // the listener, so the forwarder has to resolve its entry.
        let first_kept = log
            .lock()
            .await
            .latest_leaf(ThreadFilter::USER)
            .expect("user leaf");
        log.lock()
            .await
            .append_compaction(ThreadFilter::USER, "summary".into(), first_kept, 100, None)
            .expect("append the compaction checkpoint");
        bus.emit(AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason: aj_agent::events::CompactionReason::Manual,
            tokens_before: 100,
            tokens_after: 10,
            summary: Some("summary".into()),
            error: None,
        })
        .await
        .expect("emit compaction end");

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

        let positions = positions(&log).await;
        for persisted in &forwarded {
            let Some(entry) = &persisted.entry else {
                continue;
            };
            let index = usize::try_from(entry.seq).expect("fits usize") - 1;
            assert_eq!(entry.id, positions[index].1, "tag names its own entry");
        }
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
            ConversationEntryKind::SubAgentSpawn { .. }
        ));
    }

    #[tokio::test]
    async fn forwarder_leaves_a_summary_less_compaction_end_untagged() {
        let (_dir, log) = fresh_log();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = EventBus::new();
        let _h = bus.subscribe(persisting_forwarder(Arc::clone(&log), tx));

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("hi"),
        })
        .await
        .expect("emit user message");
        // An earlier successful compaction exists, so the gate has to be
        // the event's own summary, not the presence of a checkpoint.
        let first_kept = log
            .lock()
            .await
            .latest_leaf(ThreadFilter::USER)
            .expect("user leaf");
        log.lock()
            .await
            .append_compaction(ThreadFilter::USER, "earlier".into(), first_kept, 100, None)
            .expect("append the compaction checkpoint");
        let _ = drained(&mut rx);

        bus.emit(AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason: aj_agent::events::CompactionReason::Threshold,
            tokens_before: 100,
            tokens_after: 100,
            summary: None,
            error: Some("boom".into()),
        })
        .await
        .expect("emit failed compaction end");

        let forwarded = drained(&mut rx);
        assert_eq!(forwarded.len(), 1);
        assert!(
            forwarded[0].entry.is_none(),
            "a failed compaction appends nothing, so its event is not durable"
        );
    }

    #[tokio::test]
    async fn forwarder_survives_a_dropped_receiver() {
        let (_dir, log) = fresh_log();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = EventBus::new();
        let _h = bus.subscribe(persisting_forwarder(Arc::clone(&log), tx));
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
