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

use std::sync::{Arc, Mutex as StdMutex};

use aj_agent::BoxError;
use aj_agent::bus::Listener;
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::message::AgentMessage;
use tokio::sync::Mutex as TokioMutex;
use tokio::sync::mpsc::UnboundedSender;

use crate::log::{ConversationLog, ConversationView, EntryRef, ThreadFilter};
use crate::replay::TaggedEvent;

#[derive(Default)]
struct PersistenceFenceState {
    closed: bool,
    active: usize,
}

/// Session-lifetime fence for persistence listeners.
///
/// Event-bus emission snapshots its listeners before awaiting them, so dropping
/// a subscription cannot revoke a listener already in flight. Closing this
/// fence rejects every later invocation and [`Self::close`] waits for every
/// invocation admitted earlier. A session owner uses that barrier before
/// releasing its advisory lock, which prevents a detached task from appending
/// through an old listener after a rival writer acquires the session.
#[derive(Clone, Default)]
pub struct PersistenceFence {
    state: Arc<StdMutex<PersistenceFenceState>>,
    changed: Arc<tokio::sync::Notify>,
}

impl PersistenceFence {
    fn enter(&self) -> Option<PersistencePermit> {
        let mut state = self.state.lock().expect("persistence fence mutex poisoned");
        if state.closed {
            return None;
        }
        state.active += 1;
        Some(PersistencePermit(self.clone()))
    }

    /// Reject new listener invocations and wait until every invocation admitted
    /// before the close has returned.
    pub async fn close(&self) {
        loop {
            let changed = self.changed.notified();
            {
                let mut state = self.state.lock().expect("persistence fence mutex poisoned");
                state.closed = true;
                if state.active == 0 {
                    return;
                }
            }
            changed.await;
        }
    }

    /// Whether this fence rejects new listener invocations.
    pub fn is_closed(&self) -> bool {
        self.state
            .lock()
            .expect("persistence fence mutex poisoned")
            .closed
    }
}

struct PersistencePermit(PersistenceFence);

impl Drop for PersistencePermit {
    fn drop(&mut self) {
        let notify = {
            let mut state = self
                .0
                .state
                .lock()
                .expect("persistence fence mutex poisoned");
            state.active -= 1;
            state.closed && state.active == 0
        };
        if notify {
            self.0.changed.notify_waiters();
        }
    }
}

/// Log identity of an entry whose event has not been emitted yet.
///
/// Compaction appends its checkpoint and then emits `CompactionEnd` for
/// it. Filing the entry here lets the forwarder tag that event from the
/// append rather than inferring it at delivery time, which would race the
/// concurrent appends a background sub-agent makes.
///
/// `CompactionEnd` and its trailing `CompactionUsageUpdate` are emitted while
/// the append still holds the log guard. Otherwise another durable append can
/// land between the checkpoint and those events, making forwarded seqs
/// non-monotone or replacing the usage row's checkpoint origin. A bus listener
/// must therefore not take the log lock for either event in that sequence, or
/// it deadlocks against the emitting append.
///
/// One slot: at most one compaction runs per session at a time, and a
/// filed entry is taken by the very next `CompactionEnd`.
#[derive(Clone, Default)]
pub struct AppendHandoff {
    entry: Arc<StdMutex<Option<EntryRef>>>,
}

impl AppendHandoff {
    /// Hand `entry` to the next `CompactionEnd` the forwarder sees.
    pub fn file(&self, entry: EntryRef) {
        *self.entry.lock().expect("append handoff mutex poisoned") = Some(entry);
    }

    /// Take the filed entry, leaving the slot empty. `None` when the
    /// compaction appended nothing (it failed or was canceled).
    pub fn take(&self) -> Option<EntryRef> {
        self.entry
            .lock()
            .expect("append handoff mutex poisoned")
            .take()
    }
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
            // Only the arms that write take the lock, so the streaming
            // events (by far the most frequent) never contend for it.
            if appends(&event) {
                persist(&mut *log.lock().await, &event)?;
            }
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
/// sub-agent appends and mis-number the event (spec section 5). The one
/// durable event this listener does not append itself is `CompactionEnd`,
/// whose entry is filed on `handoff` by the compaction run.
///
/// The send is non-blocking and a closed receiver is ignored, so a slow
/// or absent consumer can never stall or fail a turn even though the bus
/// awaits this listener inline. That inline position is what gives the
/// sink its guarantee: an event it receives is already on disk.
///
/// **`sink` must be an unbounded channel.** The send above happens while
/// this listener holds the log, so a bounded channel would put a blocking
/// send under the log lock and stall every append in the session (spec
/// 6.9). Flow control belongs on the per-client queues downstream of the
/// consumer, never here.
pub fn persisting_forwarder(
    log: Arc<TokioMutex<ConversationLog>>,
    handoff: AppendHandoff,
    sink: UnboundedSender<TaggedEvent>,
) -> Listener {
    persisting_forwarder_inner(log, handoff, sink, None)
}

/// Build a persisting forwarder whose in-flight writes can be fenced before a
/// session releases its advisory lock.
pub fn fenced_persisting_forwarder(
    log: Arc<TokioMutex<ConversationLog>>,
    handoff: AppendHandoff,
    sink: UnboundedSender<TaggedEvent>,
    fence: PersistenceFence,
) -> Listener {
    persisting_forwarder_inner(log, handoff, sink, Some(fence))
}

fn persisting_forwarder_inner(
    log: Arc<TokioMutex<ConversationLog>>,
    handoff: AppendHandoff,
    sink: UnboundedSender<TaggedEvent>,
    fence: Option<PersistenceFence>,
) -> Listener {
    Arc::new(move |event: &AgentEvent| {
        let log = Arc::clone(&log);
        let handoff = handoff.clone();
        let sink = sink.clone();
        let fence = fence.clone();
        let event = event.clone();
        Box::pin(async move {
            let _permit = match fence {
                Some(fence) => match fence.enter() {
                    Some(permit) => Some(permit),
                    None => return Ok(()),
                },
                None => None,
            };
            if appends(&event) {
                // The send happens under the guard that did the append, so
                // the sink's order is the log's order. If we released the
                // lock first, another append could commit and forward a
                // higher position while this event was still in flight, and
                // the seqs a consumer sees would stop being monotone.
                let mut guard = log.lock().await;
                let entry = persist(&mut guard, &event)?;
                let _ = sink.send(TaggedEvent { entry, event });
            } else {
                // NOTE: this branch must not take the log lock. The
                // compaction run emits `CompactionEnd` and its trailing usage
                // while holding it (see [`AppendHandoff`]), so locking here
                // would deadlock against the very append the events belong to.
                // That same emit-under-the-guard keeps them ordered without a
                // lock of our own.
                let entry = match &event {
                    AgentEvent::CompactionEnd { .. } => handoff.take(),
                    _ => None,
                };
                let _ = sink.send(TaggedEvent { entry, event });
            }
            Ok(())
        })
    })
}

/// Whether [`persist`] writes an entry for `event`, which is what decides
/// whether the log lock is taken at all. Kept next to `persist` so the two
/// cannot disagree: a mismatch either misses an append or takes the lock
/// for every streaming update.
fn appends(event: &AgentEvent) -> bool {
    matches!(
        event,
        AgentEvent::SubAgentStart { .. } | AgentEvent::MessageEnd { .. }
    )
}

/// Write whatever `event` persists and return the appended entry, or
/// `None` for an event that persists nothing.
fn persist(log: &mut ConversationLog, event: &AgentEvent) -> Result<Option<EntryRef>, BoxError> {
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
            // Anchor the spawn root at the main thread's current
            // head. A sub-agent cannot spawn a sub-agent (the
            // `agent` tool is removed from its toolset), so the
            // parent is always the main thread.
            let parent_head = log.head().cloned().ok_or_else(|| {
                BoxError::from(format!(
                    "SubAgentStart: parent {parent:?} thread has no head entry to anchor child {child:?} at"
                ))
            })?;
            let appended =
                log.append_subagent_spawn(*child_n, parent_head, task, *background, settings)?;
            Ok(Some(appended))
        }
        AgentEvent::MessageEnd { agent_id, message } => {
            let appended = persist_message(log, *agent_id, message.clone())?;
            Ok(Some(appended))
        }
        _ => Ok(None),
    }
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

    use super::{
        AppendHandoff, PersistenceFence, fenced_persisting_forwarder, persistence_listener,
        persisting_forwarder,
    };
    use crate::log::{
        ConversationEntry, ConversationEntryKind, ConversationLog, ConversationView, ThreadFilter,
    };
    use crate::persistence::ConversationPersistence;
    use crate::replay::TaggedEvent;

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

    #[tokio::test]
    async fn closing_a_persistence_fence_drains_admitted_writers_and_rejects_late_snapshots() {
        let (_dir, log) = fresh_log();
        let fence = PersistenceFence::default();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let listener = fenced_persisting_forwarder(
            Arc::clone(&log),
            AppendHandoff::default(),
            tx,
            fence.clone(),
        );
        let held = log.lock().await;
        let initial_len = held.len();
        let admitted_listener = Arc::clone(&listener);
        let admitted = tokio::spawn(async move {
            admitted_listener(&AgentEvent::MessageEnd {
                agent_id: AgentId::Main,
                message: user_msg("admitted before close"),
            })
            .await
        });
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        assert!(
            !admitted.is_finished(),
            "the admitted listener is parked on the held log"
        );

        let closing_fence = fence.clone();
        let closing = tokio::spawn(async move { closing_fence.close().await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), async {
                while !closing.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "close waits for the listener invocation admitted before it"
        );

        drop(held);
        admitted
            .await
            .expect("admitted listener task")
            .expect("admitted append");
        closing.await.expect("fence close task");
        let after_admitted = log.lock().await.len();
        assert_eq!(after_admitted, initial_len + 1, "the admitted write landed");

        listener(&AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("snapshotted before close, invoked after"),
        })
        .await
        .expect("closed listener is a no-op");
        assert_eq!(
            log.lock().await.len(),
            after_admitted,
            "a listener invocation beginning after close cannot append"
        );
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
    async fn assistant_account_reaches_the_durable_log() {
        let (_dir, log) = fresh_log();
        {
            let mut log_guard = log.lock().await;
            let mut view = ConversationView::user(&mut log_guard);
            view.add_message(user_msg("hi")).expect("user msg");
        }
        let bus = EventBus::new();
        let _h = bus.subscribe(persistence_listener(Arc::clone(&log)));
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
            .latest_leaf(ThreadFilter::USER)
            .expect("user-thread head exists");
        let convo = log_guard.linearize(&head, ThreadFilter::USER);
        let Some(Message::Assistant(message)) = convo.last_message() else {
            panic!("expected assistant message");
        };
        assert_eq!(message.account.as_deref(), Some("work"));

        // MessageEnd is punctuation and flushes immediately. Reading the
        // file pins the durable field name rather than only the in-memory
        // value that the listener received.
        let jsonl = std::fs::read_to_string(log_guard.path()).expect("read session log");
        assert!(
            jsonl.contains(r#""account":"work""#),
            "the persisted assistant line must carry the account key: {jsonl}"
        );
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
    fn drained(rx: &mut tokio::sync::mpsc::UnboundedReceiver<TaggedEvent>) -> Vec<TaggedEvent> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    /// The id of the entry at 1-based append position `seq`.
    ///
    /// By index rather than by filtering the entries: a hole in the
    /// append order would shift what a filtered position names, and a tag
    /// is exactly a position.
    async fn entry_id_at(log: &Arc<TokioMutex<ConversationLog>>, seq: u64) -> Option<String> {
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
    async fn forwarder_tags_the_durable_events_and_forwards_the_rest() {
        let (_dir, log) = fresh_log();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handoff = AppendHandoff::default();
        let bus = EventBus::new();
        let _h = bus.subscribe(persisting_forwarder(Arc::clone(&log), handoff.clone(), tx));

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
        // the listener, so the run files it on the handoff.
        let first_kept = log
            .lock()
            .await
            .latest_leaf(ThreadFilter::USER)
            .expect("user leaf");
        let checkpoint = log
            .lock()
            .await
            .append_compaction(
                ThreadFilter::USER,
                "summary".into(),
                first_kept,
                100,
                None,
                None,
            )
            .expect("append the compaction checkpoint");
        handoff.file(checkpoint);
        bus.emit(AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason: aj_agent::events::CompactionReason::Manual,
            tokens_before: 100,
            tokens_after: 10,
            has_usage: false,
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

        for tagged in &forwarded {
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
            ConversationEntryKind::SubAgentSpawn { .. }
        ));
    }

    /// A failed or canceled compaction appends nothing and files nothing,
    /// so its `CompactionEnd` is not durable even though an earlier
    /// checkpoint exists in the log.
    #[tokio::test]
    async fn forwarder_leaves_a_compaction_end_that_filed_nothing_untagged() {
        let (_dir, log) = fresh_log();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let handoff = AppendHandoff::default();
        let bus = EventBus::new();
        let _h = bus.subscribe(persisting_forwarder(Arc::clone(&log), handoff.clone(), tx));

        bus.emit(AgentEvent::MessageEnd {
            agent_id: AgentId::Main,
            message: user_msg("hi"),
        })
        .await
        .expect("emit user message");
        let first_kept = log
            .lock()
            .await
            .latest_leaf(ThreadFilter::USER)
            .expect("user leaf");
        let earlier = log
            .lock()
            .await
            .append_compaction(
                ThreadFilter::USER,
                "earlier".into(),
                first_kept,
                100,
                None,
                None,
            )
            .expect("append the compaction checkpoint");
        handoff.file(earlier);
        bus.emit(AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason: aj_agent::events::CompactionReason::Manual,
            tokens_before: 100,
            tokens_after: 10,
            has_usage: false,
            summary: Some("earlier".into()),
            error: None,
        })
        .await
        .expect("emit the earlier compaction end");
        let _ = drained(&mut rx);

        bus.emit(AgentEvent::CompactionEnd {
            agent_id: AgentId::Main,
            reason: aj_agent::events::CompactionReason::Threshold,
            tokens_before: 100,
            tokens_after: 100,
            has_usage: false,
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

    /// A tool batch persists one entry per result, so the forwarder has to
    /// tag each result's `MessageEnd` with its own entry. Tagging from the
    /// log's length at delivery time would give them all the same
    /// position.
    #[tokio::test]
    async fn forwarder_tags_each_result_of_a_tool_batch_with_its_own_entry() {
        let (_dir, log) = fresh_log();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let bus = EventBus::new();
        let _h = bus.subscribe(persisting_forwarder(
            Arc::clone(&log),
            AppendHandoff::default(),
            tx,
        ));

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
        let _h = bus.subscribe(persisting_forwarder(
            Arc::clone(&log),
            AppendHandoff::default(),
            tx,
        ));
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
