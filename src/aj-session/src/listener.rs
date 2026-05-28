//! Bus listener that drives the conversation log off of
//! [`AgentEvent::MessageEnd`].
//!
//! Per `docs/aj-next-plan.md` §2.4b, the agent emits a typed
//! [`AgentEvent::MessageEnd`] for every payload that needs to hit
//! disk: the user's typed prompt, the assistant message at the end
//! of every inference, and one tool_result message per tool call in
//! a tool batch. A persistence listener subscribed to the agent's
//! bus owns the [`ConversationLog`] handle and translates each
//! [`MessageEnd`] event into one [`ConversationView::add_message`]
//! call.
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
//! thread's current head; the next [`AgentEvent::MessageEnd`]
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
use aj_agent::events::{AgentEvent, AgentId};
use aj_agent::message::AgentMessage;
use anyhow::{Result, anyhow};
use tokio::sync::Mutex as TokioMutex;

use crate::log::{ConversationLog, ConversationView, EntryId, ThreadFilter};

/// Build a [`Listener`] that writes every finalized
/// [`AgentEvent::MessageEnd`] to the given log handle.
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
    // [`AgentEvent::MessageEnd`] tagged with `Sub(n)`. A tokio mutex
    // keeps the access points async-friendly even though the map
    // itself is touched only briefly.
    let pending_anchors: Arc<TokioMutex<HashMap<usize, EntryId>>> =
        Arc::new(TokioMutex::new(HashMap::new()));

    Arc::new(move |event: &AgentEvent| {
        let log = Arc::clone(&log);
        let anchors = Arc::clone(&pending_anchors);
        let event = event.clone();
        Box::pin(async move {
            match event {
                AgentEvent::SubAgentStart { parent, child, .. } => {
                    let AgentId::Sub(child_n) = child else {
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
                AgentEvent::MessageEnd { agent_id, message } => {
                    let mut log_guard = log.lock().await;
                    let mut anchors_guard = anchors.lock().await;
                    persist(&mut log_guard, &mut anchors_guard, agent_id, message)?;
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
/// it's the sub-agent thread's own `latest_leaf` once that thread
/// has at least one entry, falling back to the captured parent
/// anchor from [`AgentEvent::SubAgentStart`] for the very first
/// write.
fn persist(
    log: &mut ConversationLog,
    pending_anchors: &mut HashMap<usize, EntryId>,
    agent_id: AgentId,
    message: AgentMessage,
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
    use aj_agent::events::{AgentEvent, AgentId};
    use aj_agent::message::AgentMessage;
    use aj_models::types::{
        AssistantContent, AssistantMessage, Message, TextContent, ToolResultMessage, UserMessage,
    };
    use tempfile::TempDir;
    use tokio::sync::Mutex as TokioMutex;

    use super::persistence_listener;
    use crate::log::{ConversationEntryKind, ConversationLog, ConversationView, ThreadFilter};
    use crate::persistence::ConversationPersistence;

    /// Set up a temp sessions dir + a fresh log with a frozen system
    /// prompt root.
    fn fresh_log() -> (TempDir, Arc<TokioMutex<ConversationLog>>) {
        let dir = TempDir::new().expect("temp dir");
        let persistence = ConversationPersistence::new(dir.path().join("threads"));
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
    async fn sub_agent_first_message_anchors_at_parent_head() {
        // The persistence listener must capture the parent's
        // `latest_leaf` from `SubAgentStart` and use it as the
        // anchor for the sub-agent's first `MessageEnd` event.
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

        bus.emit(AgentEvent::SubAgentStart {
            parent: AgentId::Main,
            child: AgentId::Sub(1),
            task: "do thing".into(),
        })
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
        assert_eq!(entries.len(), 2, "got entries: {entries:#?}");
        let first = &entries[0];
        assert_eq!(first.parent_id.as_ref(), Some(&parent_anchor));
    }

    #[tokio::test]
    async fn sub_agent_assistant_without_anchor_returns_error() {
        // No `SubAgentStart` captured beforehand: the sub-agent
        // thread has no leaf and no anchor, so the bus call should
        // fail.
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
