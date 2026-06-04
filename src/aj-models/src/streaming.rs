//! Streaming event protocol.
//!
//! The unified [`AssistantMessageEvent`] / [`AssistantMessageEventStream`]
//! protocol described in `docs/models-spec.md` §2. Every event carries an
//! owned `partial` snapshot of the in-progress assistant message, and the
//! stream terminates with exactly one of [`AssistantMessageEvent::Done`]
//! or [`AssistantMessageEvent::Error`].

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::types::{AssistantError, AssistantMessage, ErrorCategory, StopReason, ToolCall};

// ===========================================================================
// Unified streaming event protocol (docs/models-spec.md §2).
// ===========================================================================

/// Subset of [`StopReason`] valid on an [`AssistantMessageEvent::Done`] event.
///
/// Mirrors the spec constraint that successful terminations are limited to
/// `Stop`, `Length`, or `ToolUse`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DoneReason {
    Stop,
    Length,
    ToolUse,
}

impl From<DoneReason> for StopReason {
    fn from(reason: DoneReason) -> Self {
        match reason {
            DoneReason::Stop => StopReason::Stop,
            DoneReason::Length => StopReason::Length,
            DoneReason::ToolUse => StopReason::ToolUse,
        }
    }
}

/// Subset of [`StopReason`] valid on an [`AssistantMessageEvent::Error`] event.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorReason {
    /// Provider returned an error before/during streaming.
    Error,
    /// Client cancelled the request locally.
    Aborted,
}

impl From<ErrorReason> for StopReason {
    fn from(reason: ErrorReason) -> Self {
        match reason {
            ErrorReason::Error => StopReason::Error,
            ErrorReason::Aborted => StopReason::Aborted,
        }
    }
}

/// Streaming event for the unified `AssistantMessage` protocol.
///
/// Every variant carries an owned `partial` clone of the in-progress
/// [`AssistantMessage`]. Cloning per event is cheap relative to the network
/// cost of producing the deltas, and gives consumers a self-contained snapshot
/// they can hand off to UI components without sharing mutable state.
///
/// The stream is terminated by exactly one of [`Done`](Self::Done) or
/// [`Error`](Self::Error); after either is pushed, no further events may be
/// emitted (see [`AssistantMessageEventStream::push`]).
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    /// Stream has started; partial message has been initialized with the
    /// provider/model/api fields populated.
    Start { partial: AssistantMessage },

    /// A new text block started at `content_index`.
    TextStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    /// Incremental text delta appended to the block at `content_index`.
    TextDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    /// The text block at `content_index` has been finalized.
    TextEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },

    /// A new thinking block started at `content_index`.
    ThinkingStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    /// Incremental thinking delta appended to the block at `content_index`.
    ThinkingDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    /// The thinking block at `content_index` has been finalized.
    ThinkingEnd {
        content_index: usize,
        content: String,
        partial: AssistantMessage,
    },

    /// A new tool call block started at `content_index`.
    ToolCallStart {
        content_index: usize,
        partial: AssistantMessage,
    },
    /// Incremental tool call argument delta (partial JSON) for the block at
    /// `content_index`. The `partial.content[content_index]` `arguments`
    /// value should reflect the best-effort parse of the cumulative bytes.
    ToolCallDelta {
        content_index: usize,
        delta: String,
        partial: AssistantMessage,
    },
    /// The tool call block at `content_index` has been finalized with fully
    /// parsed arguments.
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
        partial: AssistantMessage,
    },

    /// Stream completed successfully. Terminal: no further events follow.
    Done {
        reason: DoneReason,
        message: AssistantMessage,
    },
    /// Stream terminated unsuccessfully. Terminal: no further events follow.
    /// `error.stop_reason` is set to either [`StopReason::Error`] or
    /// [`StopReason::Aborted`] to match `reason`.
    Error {
        reason: ErrorReason,
        error: AssistantMessage,
    },
}

impl AssistantMessageEvent {
    /// Whether this event is terminal (`Done` or `Error`). After a terminal
    /// event, no more events may be pushed onto an
    /// [`AssistantMessageEventStream`].
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Done { .. } | Self::Error { .. })
    }

    /// Build the canonical `Error { reason: Aborted, ... }` terminal event
    /// from a partial [`AssistantMessage`] captured at the moment a stream
    /// was cancelled by the client.
    ///
    /// Stamps `stop_reason = Aborted` on the message and, when the partial
    /// did not already carry an error payload, attaches an
    /// [`AssistantError`] in the [`ErrorCategory::Aborted`] category so the
    /// consumer's classification logic doesn't have to special-case the
    /// "the stream was cancelled but no upstream error was reported" shape.
    /// All accumulated text / thinking / tool-call deltas in the partial
    /// are preserved verbatim; sub-agents and persistence layers see the
    /// best-effort partial output exactly as it arrived.
    pub fn aborted(mut partial: AssistantMessage) -> Self {
        partial.stop_reason = StopReason::Aborted;
        if partial.error.is_none() {
            partial.error = Some(AssistantError::new(
                ErrorCategory::Aborted,
                "client cancelled the request",
            ));
        }
        Self::Error {
            reason: ErrorReason::Aborted,
            error: partial,
        }
    }

    /// Build the canonical `Error { reason: Error, ... }` terminal event
    /// for a stream that closed before the provider sent its terminal
    /// frame (`message_stop` / `finish_reason` / `response.completed`).
    ///
    /// A mid-stream transport drop leaves a truncated turn: the accumulated
    /// deltas are real, but the turn never actually finished. Per
    /// `docs/models-spec.md` §10.3 this is a retryable
    /// [`ErrorCategory::Transient`] failure, not a successful `Done` — so a
    /// caller's retry layer re-issues the turn instead of persisting a
    /// cut-off answer as complete. Accumulated content is preserved
    /// verbatim (mirroring [`Self::aborted`]) so a non-retrying consumer
    /// still sees best-effort output. When the partial already carries an
    /// error payload (e.g. a mid-stream `error` frame arrived but the
    /// closing terminal frame did not) that classification is kept.
    pub fn truncated(mut partial: AssistantMessage) -> Self {
        partial.stop_reason = StopReason::Error;
        if partial.error.is_none() {
            partial.error = Some(AssistantError::new(
                ErrorCategory::Transient,
                "stream ended without a terminal event",
            ));
        }
        Self::Error {
            reason: ErrorReason::Error,
            error: partial,
        }
    }

    /// Borrow the running snapshot of the partial message.
    ///
    /// For terminal events this returns the final message (the one that will
    /// also be returned by [`AssistantMessageEventStream::result`]). For all
    /// other events it returns the in-progress `partial` snapshot.
    pub fn partial(&self) -> &AssistantMessage {
        match self {
            Self::Start { partial }
            | Self::TextStart { partial, .. }
            | Self::TextDelta { partial, .. }
            | Self::TextEnd { partial, .. }
            | Self::ThinkingStart { partial, .. }
            | Self::ThinkingDelta { partial, .. }
            | Self::ThinkingEnd { partial, .. }
            | Self::ToolCallStart { partial, .. }
            | Self::ToolCallDelta { partial, .. }
            | Self::ToolCallEnd { partial, .. } => partial,
            Self::Done { message, .. } => message,
            Self::Error { error, .. } => error,
        }
    }
}

/// Outcome of [`select_cancel`].
pub enum SelectOutcome<T> {
    /// The future completed with `T` before the cancellation token fired.
    Ready(T),
    /// The cancellation token fired before the future completed; the
    /// future has been dropped.
    Cancelled,
}

/// Await `fut` concurrently with `token.cancelled()`. When `token` is
/// `None` this just awaits `fut` (the cancellation path is unreachable),
/// matching the "no cancel installed" case providers see when the
/// caller doesn't set [`StreamOptions::cancel`](crate::types::StreamOptions).
///
/// Used by every provider's `run_stream_inner` to drive the streaming
/// HTTP request inside a `select!` against the per-call cancellation
/// token so a `cancel()` rapidly tears down both the HTTP connection
/// (via dropping the SSE handle) and the polling task.
pub async fn select_cancel<T, F>(
    token: Option<&tokio_util::sync::CancellationToken>,
    fut: F,
) -> SelectOutcome<T>
where
    F: std::future::Future<Output = T>,
{
    let Some(token) = token else {
        return SelectOutcome::Ready(fut.await);
    };
    tokio::pin!(fut);
    tokio::select! {
        biased;
        _ = token.cancelled() => SelectOutcome::Cancelled,
        value = &mut fut => SelectOutcome::Ready(value),
    }
}

/// Async stream of [`AssistantMessageEvent`]s with a side channel for the
/// final [`AssistantMessage`].
///
/// The stream is shared between a single producer (provider implementation)
/// and a single consumer (typically the agent loop). The producer calls
/// [`push`](Self::push) to enqueue events and either pushes a terminal
/// [`Done`](AssistantMessageEvent::Done) /
/// [`Error`](AssistantMessageEvent::Error) event or calls
/// [`end`](Self::end) to close the stream. The consumer drives the stream
/// via the [`Stream`] impl and may also call
/// [`result`](Self::result) to await the final message.
///
/// The handle is cheap to clone — clones share the underlying state via
/// `Arc`, so a producer task can hold its own clone while the consumer owns
/// the polling handle. Only one consumer should poll the stream at a time;
/// concurrent polls panic on lock acquisition.
pub struct AssistantMessageEventStream {
    inner: Arc<EventStreamInner>,
}

struct EventStreamInner {
    /// Producer side; `None` once the stream has ended (sender dropped).
    sender: Mutex<Option<UnboundedSender<AssistantMessageEvent>>>,
    /// Consumer side; the [`Stream`] impl polls this directly.
    receiver: Mutex<UnboundedReceiver<AssistantMessageEvent>>,
    /// Final [`AssistantMessage`] populated when a terminal event is pushed
    /// or [`AssistantMessageEventStream::end`] runs without one.
    final_message: Mutex<Option<AssistantMessage>>,
    /// Wakes [`AssistantMessageEventStream::result`] futures once
    /// `final_message` is populated.
    final_notify: Notify,
    /// True once a terminal event has been pushed or `end` has been called.
    /// Subsequent pushes are dropped silently.
    terminated: AtomicBool,
}

impl Clone for AssistantMessageEventStream {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for AssistantMessageEventStream {
    fn default() -> Self {
        Self::new()
    }
}

impl AssistantMessageEventStream {
    /// Create a new, empty stream.
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            inner: Arc::new(EventStreamInner {
                sender: Mutex::new(Some(tx)),
                receiver: Mutex::new(rx),
                final_message: Mutex::new(None),
                final_notify: Notify::new(),
                terminated: AtomicBool::new(false),
            }),
        }
    }

    /// Push an event onto the stream. Pushes after a terminal event (or after
    /// [`end`](Self::end)) are dropped silently; this matches the spec rule
    /// that no further events may follow `Done` or `Error`.
    pub fn push(&self, event: AssistantMessageEvent) {
        if self.inner.terminated.load(Ordering::SeqCst) {
            return;
        }

        // Capture the final message before we move `event` into the channel.
        let terminal_final = match &event {
            AssistantMessageEvent::Done { message, .. } => Some(message.clone()),
            AssistantMessageEvent::Error { error, .. } => Some(error.clone()),
            _ => None,
        };

        // Forward to the consumer side. If the receiver has been dropped we
        // silently swallow the event — there's no useful recovery here and
        // the producer shouldn't have to care about consumer lifecycle.
        if let Some(tx) = self.inner.sender.lock().unwrap().as_ref() {
            let _ = tx.send(event);
        }

        if let Some(message) = terminal_final {
            self.terminate(Some(message));
        }
    }

    /// Close the stream without emitting a terminal event. Subsequent pushes
    /// are dropped. If no terminal event has been pushed before this call,
    /// `result()` will resolve to a synthesized error message — callers that
    /// expect a clean termination should push a `Done` or `Error` event
    /// themselves.
    pub fn end(&self) {
        self.terminate(None);
    }

    /// Await the final [`AssistantMessage`].
    ///
    /// Resolves once a terminal [`AssistantMessageEvent::Done`] or
    /// [`AssistantMessageEvent::Error`] is pushed (returning the carried
    /// message), or once [`end`](Self::end) is called without a terminal
    /// event (returning a synthesized error message describing the abrupt
    /// termination).
    pub async fn result(&self) -> AssistantMessage {
        loop {
            // Subscribe to the notify *before* we check the message slot so
            // we don't miss a wakeup that races with our check.
            let notified = self.inner.final_notify.notified();
            if let Some(message) = self.inner.final_message.lock().unwrap().clone() {
                return message;
            }
            notified.await;
        }
    }

    /// Internal helper: set `terminated`, populate `final_message` (if
    /// missing), drop the sender so consumers see end-of-stream, and wake
    /// any pending `result()` futures.
    fn terminate(&self, final_message: Option<AssistantMessage>) {
        // Race-safe: only the first caller actually terminates.
        if self.inner.terminated.swap(true, Ordering::SeqCst) {
            return;
        }

        let mut slot = self.inner.final_message.lock().unwrap();
        if slot.is_none() {
            *slot = Some(final_message.unwrap_or_else(|| {
                // The stream closed without emitting a terminal event:
                // synthesize one so callers awaiting `result()` always
                // see a typed error. Mark as transient — a stream drop
                // is recoverable from the agent's perspective.
                let mut msg = AssistantMessage::empty();
                msg.stop_reason = StopReason::Error;
                msg.error = Some(AssistantError::new(
                    ErrorCategory::Transient,
                    "stream ended without a terminal event",
                ));
                msg
            }));
        }
        drop(slot);

        // Drop the sender to close the channel; the consumer's [`Stream`]
        // impl will yield `Poll::Ready(None)` once any queued events are
        // drained.
        self.inner.sender.lock().unwrap().take();
        self.inner.final_notify.notify_waiters();
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // Single-consumer contract: lock the receiver for the duration of
        // this poll. `try_lock` would also work since concurrent polls are
        // a programmer error, but `lock` lets us panic with a better message
        // on misuse.
        let mut rx = self
            .inner
            .receiver
            .lock()
            .expect("AssistantMessageEventStream receiver mutex poisoned");
        rx.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AssistantContent, AssistantMessage, ToolCall, Usage};

    use futures::StreamExt;

    fn sample_partial() -> AssistantMessage {
        AssistantMessage {
            content: vec![],
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4".into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        }
    }

    #[tokio::test]
    async fn delivers_events_in_push_order() {
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();
        producer.push(AssistantMessageEvent::Start {
            partial: sample_partial(),
        });
        producer.push(AssistantMessageEvent::TextStart {
            content_index: 0,
            partial: sample_partial(),
        });
        producer.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "hello".into(),
            partial: sample_partial(),
        });
        let mut final_msg = sample_partial();
        final_msg.content = vec![AssistantContent::text("hello")];
        producer.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: final_msg.clone(),
        });

        let mut stream = stream;
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            let terminal = ev.is_terminal();
            events.push(ev);
            if terminal {
                break;
            }
        }
        assert_eq!(events.len(), 4);
        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        assert!(matches!(events[3], AssistantMessageEvent::Done { .. }));

        // Stream should be drained / closed now.
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn done_event_resolves_result() {
        let stream = AssistantMessageEventStream::new();
        let mut final_msg = sample_partial();
        final_msg.content = vec![AssistantContent::ToolCall(ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: serde_json::json!({"path": "/tmp"}),
        })];
        final_msg.stop_reason = StopReason::ToolUse;
        stream.push(AssistantMessageEvent::Done {
            reason: DoneReason::ToolUse,
            message: final_msg.clone(),
        });

        let result = stream.result().await;
        assert_eq!(result.stop_reason, StopReason::ToolUse);
        assert_eq!(result.content.len(), 1);
    }

    #[tokio::test]
    async fn error_event_resolves_result_with_error_detail() {
        let stream = AssistantMessageEventStream::new();
        let mut err_msg = sample_partial();
        err_msg.stop_reason = StopReason::Error;
        err_msg.error = Some(AssistantError::new(
            ErrorCategory::RateLimit,
            "rate limited",
        ));
        stream.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: err_msg.clone(),
        });

        let result = stream.result().await;
        assert_eq!(result.stop_reason, StopReason::Error);
        let err = result.error.as_ref().expect("error populated");
        assert_eq!(err.category, ErrorCategory::RateLimit);
        assert_eq!(err.message, "rate limited");
    }

    #[tokio::test]
    async fn pushes_after_terminal_are_dropped() {
        let stream = AssistantMessageEventStream::new();
        stream.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: sample_partial(),
        });
        // This second push should be a no-op.
        stream.push(AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "ignored".into(),
            partial: sample_partial(),
        });

        let mut stream = stream;
        // Should yield the Done event then close.
        let first = stream.next().await.expect("at least one event");
        assert!(matches!(first, AssistantMessageEvent::Done { .. }));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn end_without_terminal_synthesizes_error() {
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();

        // Spawn the result future before ending so we can verify the wakeup.
        let result_handle = tokio::spawn(async move { stream.result().await });

        // Give the result task a tick to register its waker.
        tokio::task::yield_now().await;
        producer.end();

        let result = result_handle.await.unwrap();
        assert_eq!(result.stop_reason, StopReason::Error);
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn end_after_terminal_keeps_existing_final_message() {
        let stream = AssistantMessageEventStream::new();
        let mut final_msg = sample_partial();
        final_msg.stop_reason = StopReason::Stop;
        stream.push(AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: final_msg.clone(),
        });
        // Calling end after a terminal event should not overwrite the
        // captured final message.
        stream.end();

        let result = stream.result().await;
        assert_eq!(result.stop_reason, StopReason::Stop);
        assert!(result.error.is_none());
    }

    #[test]
    fn done_reason_maps_to_stop_reason() {
        assert_eq!(StopReason::from(DoneReason::Stop), StopReason::Stop);
        assert_eq!(StopReason::from(DoneReason::Length), StopReason::Length);
        assert_eq!(StopReason::from(DoneReason::ToolUse), StopReason::ToolUse);
    }

    #[test]
    fn error_reason_maps_to_stop_reason() {
        assert_eq!(StopReason::from(ErrorReason::Error), StopReason::Error);
        assert_eq!(StopReason::from(ErrorReason::Aborted), StopReason::Aborted);
    }

    #[test]
    fn truncated_sets_transient_error_and_preserves_content() {
        let mut partial = sample_partial();
        partial.content = vec![AssistantContent::text("partial answer")];
        partial.stop_reason = StopReason::Stop;

        let event = AssistantMessageEvent::truncated(partial);
        match event {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(reason, ErrorReason::Error);
                assert_eq!(error.stop_reason, StopReason::Error);
                // Accumulated deltas survive so a non-retrying consumer
                // still sees the best-effort partial output.
                assert_eq!(error.content.len(), 1);
                let err = error.error.as_ref().expect("transient error attached");
                assert_eq!(err.category, ErrorCategory::Transient);
            }
            other => panic!("expected Error terminal, got {other:?}"),
        }
    }

    #[test]
    fn truncated_keeps_existing_error_payload() {
        let mut partial = sample_partial();
        partial.error = Some(AssistantError::new(ErrorCategory::ContentFilter, "blocked"));

        if let AssistantMessageEvent::Error { error, .. } =
            AssistantMessageEvent::truncated(partial)
        {
            let err = error.error.as_ref().expect("error preserved");
            assert_eq!(err.category, ErrorCategory::ContentFilter);
            assert_eq!(err.message, "blocked");
        } else {
            panic!("expected Error terminal");
        }
    }

    #[test]
    fn partial_accessor_returns_consistent_snapshot() {
        let msg = sample_partial();
        let event = AssistantMessageEvent::TextDelta {
            content_index: 0,
            delta: "x".into(),
            partial: msg.clone(),
        };
        assert_eq!(event.partial().model, msg.model);

        let done = AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: msg.clone(),
        };
        assert!(done.is_terminal());
        assert_eq!(done.partial().model, msg.model);
    }
}
