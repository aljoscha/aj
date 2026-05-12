//! Scripted [`Provider`] implementation for tests, demos, and TUI eyeballing.
//!
//! [`ScriptedProvider`] is a [`Provider`] that replays canned
//! [`AssistantMessageEvent`] sequences instead of calling out to a real LLM
//! provider. It mirrors the legacy [`crate::scripted::ScriptedModel`] (kept
//! alive while the migration in `docs/aj-next-progress.md` Phase 6 rolls
//! out) but speaks the unified streaming protocol from
//! `docs/models-spec.md` §2.
//!
//! It serves two audiences:
//!
//! - **Tests.** The agent loop's snapshot tests build a [`ProviderScript`]
//!   per inference and assert on the resulting events.  Tests want strict
//!   behaviour: if the agent runs more inferences than scripted, the
//!   provider panics so the regression is loud
//!   ([`ExhaustedBehavior::Panic`]).
//! - **Demos / `--scripted` CLI mode.** The binary plugs `ScriptedProvider`
//!   in place of the real provider so users can eyeball the TUI's rendering
//!   of thinking blocks, tool calls, errors, retries, etc., without a
//!   network round-trip. Demos run for an unknown number of turns (the
//!   user can chat freely after the canned script finishes), so the
//!   provider defaults to a lenient end-of-turn fallback
//!   ([`ExhaustedBehavior::EndTurn`]) once the script queue is exhausted.
//!
//! Two authoring vocabularies are provided:
//!
//! - [`ScriptBuilder`] — builds a [`ProviderScript`] step-by-step, threading
//!   an in-progress [`AssistantMessage`] partial through each event. Use
//!   this when you want to write per-token deltas (TUI demos that exercise
//!   the streaming render path).
//! - [`script_from_message`] — auto-generates a complete event sequence
//!   from a static [`AssistantMessage`]. Use this when the test cares
//!   about the agent's behaviour on the finalized message and not the
//!   streaming shape (the bulk of `event_protocol_tests`'s scripts).

use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

pub mod demos;

use crate::provider::Provider;
use crate::registry::ModelInfo;
use crate::streaming::{
    AssistantMessageEvent, AssistantMessageEventStream, DoneReason, ErrorReason,
};
use crate::types::{
    AssistantContent, AssistantError, AssistantMessage, Context, ErrorCategory,
    SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingContent, ToolCall,
};

// ===========================================================================
// Script / step types
// ===========================================================================

/// A single streaming event with an optional pre-emit delay.
#[derive(Clone, Debug)]
pub struct ProviderScriptStep {
    /// How long to sleep before emitting [`Self::event`].
    pub delay: Duration,
    pub event: AssistantMessageEvent,
}

impl ProviderScriptStep {
    pub fn new(delay: Duration, event: AssistantMessageEvent) -> Self {
        Self { delay, event }
    }

    /// Step with no pre-emit delay.
    pub fn immediate(event: AssistantMessageEvent) -> Self {
        Self {
            delay: Duration::ZERO,
            event,
        }
    }
}

/// One inference's worth of scripted events.
///
/// The agent issues one [`Provider::stream`] call per loop iteration. Each
/// call consumes exactly one [`ProviderScript`] from the provider's queue
/// and replays its [`ProviderScriptStep`]s in order, terminating with the
/// last step's event (which should be [`AssistantMessageEvent::Done`] or
/// [`AssistantMessageEvent::Error`] per the unified protocol's terminal
/// invariant).
#[derive(Clone, Debug, Default)]
pub struct ProviderScript {
    pub steps: Vec<ProviderScriptStep>,
}

impl ProviderScript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a script from a flat list of [`AssistantMessageEvent`]s with no
    /// inter-event delay. Convenient for snapshot tests where the timing
    /// is irrelevant.
    pub fn from_events(events: Vec<AssistantMessageEvent>) -> Self {
        Self {
            steps: events
                .into_iter()
                .map(ProviderScriptStep::immediate)
                .collect(),
        }
    }

    /// Append an event after a delay.
    pub fn push(mut self, delay: Duration, event: AssistantMessageEvent) -> Self {
        self.steps.push(ProviderScriptStep::new(delay, event));
        self
    }

    /// Append an event with no delay.
    pub fn push_immediate(mut self, event: AssistantMessageEvent) -> Self {
        self.steps.push(ProviderScriptStep::immediate(event));
        self
    }
}

/// What [`ScriptedProvider`] does when the agent asks for more inferences
/// than the queued [`ProviderScript`]s supply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExhaustedBehavior {
    /// Panic the inference task. Used by tests where an extra inference
    /// indicates a regression that needs to be diagnosed immediately.
    Panic,
    /// Emit a minimal "end turn" finalized message so the agent loop
    /// terminates cleanly. Used by demos and the `--scripted` CLI flag,
    /// where the user may chat freely with the model after the canned
    /// script runs out.
    EndTurn,
}

// ===========================================================================
// ScriptedProvider
// ===========================================================================

/// A [`Provider`] backed by a queue of [`ProviderScript`]s.
///
/// Cheap to construct; cheap to clone its events (every event clones in
/// O(payload) bytes). The script queue lives behind a [`Mutex`] so the
/// trait's `&self` receiver can dequeue each call's script in turn.
///
/// The provider stamps the requested model's `api` / `provider` / `id`
/// identifiers on the synthesized exhausted-fallback message so attribution
/// stays correct even when the script queue is empty.
pub struct ScriptedProvider {
    scripts: Mutex<std::vec::IntoIter<ProviderScript>>,
    on_exhausted: ExhaustedBehavior,
}

impl ScriptedProvider {
    /// Build a scripted provider from a list of per-inference scripts.
    ///
    /// Defaults to [`ExhaustedBehavior::EndTurn`] (demo-friendly). Use the
    /// builder methods to customise.
    pub fn new(scripts: Vec<ProviderScript>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter()),
            on_exhausted: ExhaustedBehavior::EndTurn,
        }
    }

    /// Convenience: build a scripted provider from a list of plain event
    /// vectors, no delays. Equivalent to
    /// `ScriptedProvider::new(events.into_iter().map(ProviderScript::from_events).collect())`.
    pub fn from_event_vecs(scripts: Vec<Vec<AssistantMessageEvent>>) -> Self {
        Self::new(
            scripts
                .into_iter()
                .map(ProviderScript::from_events)
                .collect(),
        )
    }

    /// "Final message script" mode: build a scripted provider from a list
    /// of static [`AssistantMessage`]s, auto-generating the streaming
    /// event sequence for each.
    ///
    /// Each message becomes one script: a `Start` event, then for each
    /// content block a `*Start` / `*Delta` / `*End` triplet (with text
    /// content split into [`chunk_size`]-character chunks; see
    /// [`script_from_message`] for details), then a terminal `Done` event
    /// derived from the message's `stop_reason`.
    ///
    /// Saves test authors from writing per-token deltas when they only
    /// care about the agent's behaviour on the finalized message.
    pub fn from_messages(
        messages: Vec<AssistantMessage>,
        chunk_size: usize,
        chunk_delay: Duration,
    ) -> Self {
        Self::new(
            messages
                .into_iter()
                .map(|m| script_from_message(m, chunk_size, chunk_delay))
                .collect(),
        )
    }

    /// Override the exhausted-queue behaviour.
    pub fn on_exhausted(mut self, behavior: ExhaustedBehavior) -> Self {
        self.on_exhausted = behavior;
        self
    }
}

impl Provider for ScriptedProvider {
    fn stream(
        &self,
        model: &ModelInfo,
        _context: &Context,
        _options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        let next = self.scripts.lock().unwrap().next();
        match next {
            Some(script) => spawn_script(script, model.clone()),
            None => match self.on_exhausted {
                ExhaustedBehavior::Panic => {
                    panic!("ScriptedProvider exhausted: agent ran more inferences than scripted");
                }
                ExhaustedBehavior::EndTurn => spawn_empty_done(model.clone()),
            },
        }
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        // Reasoning options are irrelevant to the scripted protocol —
        // events were prebaked at authoring time. Delegate to `stream`
        // with the base options to keep the surface uniform.
        self.stream(model, context, &options.base)
    }
}

// ---------------------------------------------------------------------------
// Stream driver
// ---------------------------------------------------------------------------

/// Spawn a tokio task that drains `script` onto a fresh stream, honouring
/// the per-step delays. Returns the consumer-side handle.
fn spawn_script(script: ProviderScript, _model: ModelInfo) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let producer = stream.clone();
    tokio::spawn(async move {
        let mut saw_terminal = false;
        for step in script.steps {
            if !step.delay.is_zero() {
                tokio::time::sleep(step.delay).await;
            }
            let is_terminal = step.event.is_terminal();
            producer.push(step.event);
            if is_terminal {
                saw_terminal = true;
                break;
            }
        }
        if !saw_terminal {
            // Safety net: ensure the stream always terminates. The
            // synthesized error mirrors the spec contract that
            // dropping a stream without a terminal event yields a
            // transient-category failure.
            producer.end();
        }
    });
    stream
}

/// Spawn an immediately-terminating stream carrying a minimal `Done`
/// event. Used by [`ExhaustedBehavior::EndTurn`] so demos that run out of
/// script still let the agent loop exit cleanly.
fn spawn_empty_done(model: ModelInfo) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let mut message = AssistantMessage::empty();
    message.api = model.api.clone();
    message.provider = model.provider.clone();
    message.model = model.id.clone();
    message.stop_reason = StopReason::Stop;
    stream.push(AssistantMessageEvent::Done {
        reason: DoneReason::Stop,
        message,
    });
    stream
}

// ===========================================================================
// ScriptBuilder — manual authoring vocabulary
// ===========================================================================

/// Helper that builds a [`ProviderScript`] step-by-step while threading the
/// in-progress [`AssistantMessage`] partial through each event.
///
/// Use this when you want to author per-token deltas (TUI demos, streaming
/// renderer tests). For "I just want the agent to see this finalized
/// message" use [`script_from_message`] instead.
///
/// Each `*_block` method advances the builder's `next_content_index`,
/// emits the `*Start` event, one or more `*Delta` events (chunked off the
/// content at [`Self::chunk_size`] characters), and the `*End` event. The
/// running partial is cloned into every event so consumers always see a
/// coherent snapshot.
///
/// Call [`Self::done`] or [`Self::error`] to finalize; both append the
/// matching terminal event and return the script.
pub struct ScriptBuilder {
    /// Cumulative partial; cloned into each event.
    partial: AssistantMessage,
    /// Pending steps for the script under construction.
    steps: Vec<ProviderScriptStep>,
    /// Delay attached to the next step pushed. Reset to `Duration::ZERO`
    /// after each step.
    pending_delay: Duration,
    /// Index assigned to the next content block.
    next_content_index: usize,
    /// Default chunk size (characters per delta) for `*_block` helpers.
    /// `0` means "emit a single delta with the full content".
    chunk_size: usize,
    /// Default per-chunk delay for `*_block` helpers.
    chunk_delay: Duration,
}

impl ScriptBuilder {
    /// Start a new builder. The provided identity fields are stamped onto
    /// the running partial and inherited by every emitted event.
    pub fn new(
        api: impl Into<String>,
        provider: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        let mut partial = AssistantMessage::empty();
        partial.api = api.into();
        partial.provider = provider.into();
        partial.model = model.into();
        Self {
            partial,
            steps: Vec::new(),
            pending_delay: Duration::ZERO,
            next_content_index: 0,
            chunk_size: 0,
            chunk_delay: Duration::ZERO,
        }
    }

    /// Default chunk size used by `*_block` helpers when no per-call
    /// override is supplied. `0` means "single delta with full content".
    pub fn with_chunk_size(mut self, chunk_size: usize) -> Self {
        self.chunk_size = chunk_size;
        self
    }

    /// Default per-chunk delay used by `*_block` helpers when no per-call
    /// override is supplied.
    pub fn with_chunk_delay(mut self, chunk_delay: Duration) -> Self {
        self.chunk_delay = chunk_delay;
        self
    }

    /// Schedule a delay before the next emitted step. Applies once, then
    /// resets. Useful for inserting a section break ahead of the next
    /// block.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.pending_delay = delay;
        self
    }

    /// Push the opening [`AssistantMessageEvent::Start`] event.
    ///
    /// Most builders want this as their first call so the stream begins
    /// with a partial snapshot the consumer can latch onto. Optional for
    /// builders that intentionally skip it (e.g. to exercise the
    /// agent's tolerance for missing `Start`).
    pub fn start(mut self) -> Self {
        let event = AssistantMessageEvent::Start {
            partial: self.partial.clone(),
        };
        self.push_step(event);
        self
    }

    /// Append a text block. Emits `TextStart`, then one delta per chunk,
    /// then `TextEnd`. Returns `self` for chaining.
    pub fn text_block(self, text: impl AsRef<str>) -> Self {
        let chunk_size = self.chunk_size;
        let chunk_delay = self.chunk_delay;
        self.text_block_chunked(text, chunk_size, chunk_delay)
    }

    /// Append a text block with a per-call chunk size and chunk delay.
    pub fn text_block_chunked(
        mut self,
        text: impl AsRef<str>,
        chunk_size: usize,
        chunk_delay: Duration,
    ) -> Self {
        let text = text.as_ref();
        let idx = self.next_content_index;
        self.next_content_index += 1;

        // Start: place an empty text block at `idx` in the partial.
        self.partial
            .content
            .push(AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: None,
            }));
        let start_event = AssistantMessageEvent::TextStart {
            content_index: idx,
            partial: self.partial.clone(),
        };
        self.push_step(start_event);

        // Deltas: walk the text in chunks, updating the partial's text
        // field as we go so each delta carries a coherent snapshot.
        for chunk in split_chunks(text, chunk_size) {
            if let Some(AssistantContent::Text(t)) = self.partial.content.get_mut(idx) {
                t.text.push_str(chunk);
            }
            let event = AssistantMessageEvent::TextDelta {
                content_index: idx,
                delta: chunk.to_string(),
                partial: self.partial.clone(),
            };
            self.steps.push(ProviderScriptStep::new(chunk_delay, event));
        }

        // End: nothing to update on the partial — the text is already
        // populated from the deltas.
        let end_event = AssistantMessageEvent::TextEnd {
            content_index: idx,
            content: text.to_string(),
            partial: self.partial.clone(),
        };
        self.steps.push(ProviderScriptStep::immediate(end_event));
        self
    }

    /// Append a thinking block. `signature` is the opaque provider-specific
    /// signature attached to the finalized thinking content (Anthropic
    /// requires this for multi-turn replay; OpenAI uses it to store the
    /// reasoning item ID).
    pub fn thinking_block(self, thinking: impl AsRef<str>, signature: Option<String>) -> Self {
        let chunk_size = self.chunk_size;
        let chunk_delay = self.chunk_delay;
        self.thinking_block_chunked(thinking, signature, chunk_size, chunk_delay)
    }

    /// Append a thinking block with a per-call chunk size and chunk delay.
    pub fn thinking_block_chunked(
        mut self,
        thinking: impl AsRef<str>,
        signature: Option<String>,
        chunk_size: usize,
        chunk_delay: Duration,
    ) -> Self {
        let thinking = thinking.as_ref();
        let idx = self.next_content_index;
        self.next_content_index += 1;

        // Start: place an empty thinking block at `idx`.
        self.partial
            .content
            .push(AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: signature.clone(),
                redacted: false,
            }));
        let start_event = AssistantMessageEvent::ThinkingStart {
            content_index: idx,
            partial: self.partial.clone(),
        };
        self.push_step(start_event);

        // Deltas: same chunking scheme as text.
        for chunk in split_chunks(thinking, chunk_size) {
            if let Some(AssistantContent::Thinking(th)) = self.partial.content.get_mut(idx) {
                th.thinking.push_str(chunk);
            }
            let event = AssistantMessageEvent::ThinkingDelta {
                content_index: idx,
                delta: chunk.to_string(),
                partial: self.partial.clone(),
            };
            self.steps.push(ProviderScriptStep::new(chunk_delay, event));
        }

        let end_event = AssistantMessageEvent::ThinkingEnd {
            content_index: idx,
            content: thinking.to_string(),
            partial: self.partial.clone(),
        };
        self.steps.push(ProviderScriptStep::immediate(end_event));
        self
    }

    /// Append a tool call block. The argument JSON is serialized once and
    /// chunked across the deltas, so consumers exercising the partial-JSON
    /// parser see incremental input.
    pub fn tool_call_block(
        self,
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
    ) -> Self {
        let chunk_size = self.chunk_size;
        let chunk_delay = self.chunk_delay;
        self.tool_call_block_chunked(id, name, arguments, chunk_size, chunk_delay)
    }

    /// Append a tool call block with a per-call chunk size and chunk delay.
    pub fn tool_call_block_chunked(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: Value,
        chunk_size: usize,
        chunk_delay: Duration,
    ) -> Self {
        let id = id.into();
        let name = name.into();
        let idx = self.next_content_index;
        self.next_content_index += 1;

        // Start: place a tool call with empty args in the partial. The
        // unified protocol's ToolCallStart carries no payload beyond the
        // index; the partial gives consumers the call id/name as soon as
        // they're known.
        self.partial
            .content
            .push(AssistantContent::ToolCall(ToolCall {
                id: id.clone(),
                name: name.clone(),
                arguments: Value::Null,
            }));
        let start_event = AssistantMessageEvent::ToolCallStart {
            content_index: idx,
            partial: self.partial.clone(),
        };
        self.push_step(start_event);

        // Deltas: serialize the arguments and chunk the JSON. The partial's
        // `arguments` field is updated to a best-effort parse of the
        // cumulative bytes so far so consumers can render live argument
        // previews; on the final delta it matches the full value.
        let serialized = serde_json::to_string(&arguments).unwrap_or_else(|_| "{}".to_string());
        let mut buffered = String::new();
        let chunks: Vec<&str> = split_chunks(&serialized, chunk_size);
        let last_index = chunks.len().saturating_sub(1);
        for (i, chunk) in chunks.iter().enumerate() {
            buffered.push_str(chunk);
            // On the final chunk, plug in the fully parsed arguments; on
            // intermediate chunks try a best-effort parse and fall back to
            // `Value::Null` if it doesn't parse yet.
            let parsed = if i == last_index {
                arguments.clone()
            } else {
                serde_json::from_str(&buffered).unwrap_or(Value::Null)
            };
            if let Some(AssistantContent::ToolCall(tc)) = self.partial.content.get_mut(idx) {
                tc.arguments = parsed;
            }
            let event = AssistantMessageEvent::ToolCallDelta {
                content_index: idx,
                delta: (*chunk).to_string(),
                partial: self.partial.clone(),
            };
            self.steps.push(ProviderScriptStep::new(chunk_delay, event));
        }

        let tool_call = ToolCall {
            id,
            name,
            arguments,
        };
        let end_event = AssistantMessageEvent::ToolCallEnd {
            content_index: idx,
            tool_call,
            partial: self.partial.clone(),
        };
        self.steps.push(ProviderScriptStep::immediate(end_event));
        self
    }

    /// Finalize the script with a [`AssistantMessageEvent::Done`] event.
    ///
    /// The terminal message captures the current partial plus the chosen
    /// stop reason. Per the unified spec, `Done`'s `reason` is the
    /// successful subset of [`StopReason`] ([`DoneReason::Stop`],
    /// [`DoneReason::Length`], [`DoneReason::ToolUse`]); the message's
    /// `stop_reason` field is set to match.
    pub fn done(mut self, reason: DoneReason) -> ProviderScript {
        let mut message = self.partial.clone();
        message.stop_reason = reason.into();
        let event = AssistantMessageEvent::Done { reason, message };
        self.push_step(event);
        ProviderScript { steps: self.steps }
    }

    /// Finalize the script with a [`AssistantMessageEvent::Error`] event.
    ///
    /// The terminal message captures the current partial, the chosen
    /// error reason, and the supplied [`AssistantError`] payload.
    pub fn error(mut self, reason: ErrorReason, error: AssistantError) -> ProviderScript {
        let mut message = self.partial.clone();
        message.stop_reason = reason.into();
        message.error = Some(error);
        let event = AssistantMessageEvent::Error {
            reason,
            error: message,
        };
        self.push_step(event);
        ProviderScript { steps: self.steps }
    }

    /// Internal helper: push a step honouring (and consuming) the
    /// `pending_delay` slot.
    fn push_step(&mut self, event: AssistantMessageEvent) {
        let delay = std::mem::replace(&mut self.pending_delay, Duration::ZERO);
        self.steps.push(ProviderScriptStep::new(delay, event));
    }
}

// ===========================================================================
// from_message — auto-generation mode
// ===========================================================================

/// Auto-generate a [`ProviderScript`] from a static [`AssistantMessage`].
///
/// Walks the message's content blocks in order, emitting the matching
/// `*Start` / `*Delta` / `*End` triple for each, and finishes with a
/// terminal event derived from `message.stop_reason`:
///
/// - `Stop` / `Length` / `ToolUse` → [`AssistantMessageEvent::Done`].
/// - `Error` → [`AssistantMessageEvent::Error`] with reason
///   [`ErrorReason::Error`] and the message's `error` field forwarded.
/// - `Aborted` → [`AssistantMessageEvent::Error`] with reason
///   [`ErrorReason::Aborted`].
///
/// `chunk_size` is forwarded to the builder's chunking helpers
/// ([`split_chunks`]): `0` means "one delta per block carrying the full
/// content", any positive value chunks the content. `chunk_delay` applies
/// to each delta.
///
/// The message's identity fields (`api`, `provider`, `model`,
/// `response_id`, `usage`, `timestamp`) ride through onto the terminal
/// event's message, so test assertions on those fields survive the
/// round-trip.
pub fn script_from_message(
    message: AssistantMessage,
    chunk_size: usize,
    chunk_delay: Duration,
) -> ProviderScript {
    let mut builder = ScriptBuilder::new(&message.api, &message.provider, &message.model)
        .with_chunk_size(chunk_size)
        .with_chunk_delay(chunk_delay);

    // The builder's partial doesn't include the source message's
    // response_id / usage / timestamp; thread them through so the
    // terminal event reproduces the message exactly.
    builder.partial.response_id = message.response_id.clone();
    builder.partial.usage = message.usage.clone();
    builder.partial.timestamp = message.timestamp;

    builder = builder.start();

    for block in &message.content {
        builder = match block {
            AssistantContent::Text(t) => builder.text_block(&t.text),
            AssistantContent::Thinking(th) => {
                builder.thinking_block(&th.thinking, th.thinking_signature.clone())
            }
            AssistantContent::ToolCall(tc) => {
                builder.tool_call_block(&tc.id, &tc.name, tc.arguments.clone())
            }
        };
    }

    // Map the source message's StopReason onto a terminal event.
    match message.stop_reason {
        StopReason::Stop => builder.done(DoneReason::Stop),
        StopReason::Length => builder.done(DoneReason::Length),
        StopReason::ToolUse => builder.done(DoneReason::ToolUse),
        StopReason::Error => builder.error(
            ErrorReason::Error,
            message.error.unwrap_or_else(|| {
                AssistantError::new(
                    ErrorCategory::Transient,
                    "scripted: synthetic error (no detail supplied)",
                )
            }),
        ),
        StopReason::Aborted => builder.error(
            ErrorReason::Aborted,
            message.error.unwrap_or_else(|| {
                AssistantError::new(ErrorCategory::Transient, "scripted: synthetic abort")
            }),
        ),
    }
}

// ===========================================================================
// Helpers
// ===========================================================================

/// Split `text` into roughly `chunk_size`-character pieces, never inside a
/// multi-byte UTF-8 scalar.
///
/// `chunk_size == 0` is a special case meaning "emit the entire text as a
/// single piece" — useful for the `from_message` mode when callers don't
/// care about token-level chunking. Empty input yields no chunks.
fn split_chunks(text: &str, chunk_size: usize) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    if chunk_size == 0 {
        return vec![text];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    while start < bytes.len() {
        let remaining = &text[start..];
        let take_bytes = remaining
            .char_indices()
            .nth(chunk_size)
            .map(|(idx, _)| idx)
            .unwrap_or(remaining.len());
        out.push(&text[start..start + take_bytes]);
        start += take_bytes;
    }
    out
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{InputModality, ModelCost};
    use futures::StreamExt;

    fn fake_model() -> ModelInfo {
        ModelInfo {
            id: "scripted-test".into(),
            name: "Scripted".into(),
            api: "scripted".into(),
            provider: "scripted".into(),
            base_url: "scripted://internal".into(),
            reasoning: false,
            supports_xhigh: false,
            supports_adaptive_thinking: false,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 1024,
            max_tokens: 256,
            headers: None,
        }
    }

    fn collect_events(
        mut stream: AssistantMessageEventStream,
    ) -> impl std::future::Future<Output = Vec<AssistantMessageEvent>> {
        async move {
            let mut out = Vec::new();
            while let Some(ev) = stream.next().await {
                out.push(ev);
            }
            out
        }
    }

    #[test]
    fn split_chunks_zero_returns_single_piece() {
        assert_eq!(split_chunks("hello", 0), vec!["hello"]);
    }

    #[test]
    fn split_chunks_empty_returns_no_pieces() {
        assert!(split_chunks("", 0).is_empty());
        assert!(split_chunks("", 4).is_empty());
    }

    #[test]
    fn split_chunks_does_not_break_multi_byte_scalars() {
        // Each grapheme here is a multi-byte scalar; splitting at byte
        // boundaries would produce invalid UTF-8.
        let pieces = split_chunks("héllo", 2);
        assert!(
            pieces
                .iter()
                .all(|p| std::str::from_utf8(p.as_bytes()).is_ok())
        );
        assert_eq!(pieces.concat(), "héllo");
    }

    #[tokio::test]
    async fn provider_replays_canned_events_in_order() {
        let mut msg = AssistantMessage::empty();
        msg.api = "scripted".into();
        msg.provider = "scripted".into();
        msg.model = "scripted-test".into();
        msg.stop_reason = StopReason::Stop;

        let script = ProviderScript::from_events(vec![
            AssistantMessageEvent::Start {
                partial: msg.clone(),
            },
            AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                message: msg.clone(),
            },
        ]);
        let provider = ScriptedProvider::new(vec![script]);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        assert!(matches!(events[1], AssistantMessageEvent::Done { .. }));
    }

    #[tokio::test]
    async fn from_messages_auto_generates_full_event_sequence() {
        let mut msg = AssistantMessage::empty();
        msg.api = "scripted".into();
        msg.provider = "scripted".into();
        msg.model = "scripted-test".into();
        msg.stop_reason = StopReason::Stop;
        msg.content.push(AssistantContent::Text(TextContent {
            text: "hello world".into(),
            text_signature: None,
        }));

        let provider = ScriptedProvider::from_messages(vec![msg], 0, Duration::ZERO);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        // Expect Start, TextStart, TextDelta, TextEnd, Done.
        assert_eq!(events.len(), 5);
        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        assert!(matches!(
            events[1],
            AssistantMessageEvent::TextStart {
                content_index: 0,
                ..
            }
        ));
        match &events[2] {
            AssistantMessageEvent::TextDelta {
                content_index,
                delta,
                ..
            } => {
                assert_eq!(*content_index, 0);
                assert_eq!(delta, "hello world");
            }
            other => panic!("expected TextDelta, got {other:?}"),
        }
        assert!(matches!(
            events[3],
            AssistantMessageEvent::TextEnd {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(
            events[4],
            AssistantMessageEvent::Done {
                reason: DoneReason::Stop,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn from_messages_chunks_text_when_chunk_size_positive() {
        let mut msg = AssistantMessage::empty();
        msg.api = "scripted".into();
        msg.provider = "scripted".into();
        msg.model = "scripted-test".into();
        msg.stop_reason = StopReason::Stop;
        msg.content.push(AssistantContent::Text(TextContent {
            text: "abcdefgh".into(),
            text_signature: None,
        }));

        // chunk_size = 3 splits "abcdefgh" into "abc" / "def" / "gh"
        let provider = ScriptedProvider::from_messages(vec![msg], 3, Duration::ZERO);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        // Start + TextStart + 3 deltas + TextEnd + Done = 7
        assert_eq!(events.len(), 7);
        let deltas: Vec<&str> = events
            .iter()
            .filter_map(|ev| match ev {
                AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deltas, vec!["abc", "def", "gh"]);
    }

    #[tokio::test]
    async fn from_messages_emits_thinking_and_tool_call_blocks() {
        let mut msg = AssistantMessage::empty();
        msg.api = "scripted".into();
        msg.provider = "scripted".into();
        msg.model = "scripted-test".into();
        msg.stop_reason = StopReason::ToolUse;
        msg.content
            .push(AssistantContent::Thinking(ThinkingContent {
                thinking: "let me think".into(),
                thinking_signature: Some("sig-1".into()),
                redacted: false,
            }));
        msg.content.push(AssistantContent::ToolCall(ToolCall {
            id: "tc-1".into(),
            name: "ping".into(),
            arguments: serde_json::json!({"foo": "bar"}),
        }));

        let provider = ScriptedProvider::from_messages(vec![msg], 0, Duration::ZERO);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        // Start + ThinkingStart + ThinkingDelta + ThinkingEnd
        // + ToolCallStart + ToolCallDelta + ToolCallEnd + Done = 8
        assert_eq!(events.len(), 8);
        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        assert!(matches!(
            events[1],
            AssistantMessageEvent::ThinkingStart {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(
            events[2],
            AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(
            events[3],
            AssistantMessageEvent::ThinkingEnd {
                content_index: 0,
                ..
            }
        ));
        assert!(matches!(
            events[4],
            AssistantMessageEvent::ToolCallStart {
                content_index: 1,
                ..
            }
        ));
        match &events[5] {
            AssistantMessageEvent::ToolCallDelta { content_index, .. } => {
                assert_eq!(*content_index, 1);
            }
            other => panic!("expected ToolCallDelta, got {other:?}"),
        }
        match &events[6] {
            AssistantMessageEvent::ToolCallEnd {
                content_index,
                tool_call,
                ..
            } => {
                assert_eq!(*content_index, 1);
                assert_eq!(tool_call.id, "tc-1");
                assert_eq!(tool_call.name, "ping");
                assert_eq!(tool_call.arguments, serde_json::json!({"foo": "bar"}));
            }
            other => panic!("expected ToolCallEnd, got {other:?}"),
        }
        match &events[7] {
            AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(*reason, DoneReason::ToolUse);
                assert_eq!(message.stop_reason, StopReason::ToolUse);
                assert_eq!(message.content.len(), 2);
            }
            other => panic!("expected Done(ToolUse), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn from_messages_maps_error_stop_reason_to_error_event() {
        let mut msg = AssistantMessage::empty();
        msg.api = "scripted".into();
        msg.provider = "scripted".into();
        msg.model = "scripted-test".into();
        msg.stop_reason = StopReason::Error;
        msg.error = Some(AssistantError::new(ErrorCategory::Transient, "boom"));

        let provider = ScriptedProvider::from_messages(vec![msg], 0, Duration::ZERO);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        let last = events.last().expect("at least one event");
        match last {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(*reason, ErrorReason::Error);
                assert_eq!(error.stop_reason, StopReason::Error);
                let err = error.error.as_ref().expect("error populated");
                assert_eq!(err.category, ErrorCategory::Transient);
                assert_eq!(err.message, "boom");
            }
            other => panic!("expected Error event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn from_messages_maps_aborted_stop_reason_to_aborted_error() {
        let mut msg = AssistantMessage::empty();
        msg.api = "scripted".into();
        msg.provider = "scripted".into();
        msg.model = "scripted-test".into();
        msg.stop_reason = StopReason::Aborted;

        let provider = ScriptedProvider::from_messages(vec![msg], 0, Duration::ZERO);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        let last = events.last().expect("at least one event");
        match last {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(*reason, ErrorReason::Aborted);
                assert_eq!(error.stop_reason, StopReason::Aborted);
            }
            other => panic!("expected aborted Error event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exhausted_endturn_emits_synthetic_done() {
        let provider = ScriptedProvider::new(Vec::new());
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(*reason, DoneReason::Stop);
                // Synthetic message inherits the requested model identity.
                assert_eq!(message.api, "scripted");
                assert_eq!(message.provider, "scripted");
                assert_eq!(message.model, "scripted-test");
            }
            other => panic!("expected synthetic Done, got {other:?}"),
        }
    }

    #[tokio::test]
    #[should_panic(expected = "ScriptedProvider exhausted")]
    async fn exhausted_panic_aborts_inference() {
        let provider = ScriptedProvider::new(Vec::new()).on_exhausted(ExhaustedBehavior::Panic);
        // Calling stream() must panic synchronously; we don't expect a
        // stream value back.
        let _ = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
    }

    #[tokio::test]
    async fn builder_text_block_threads_partial_through_deltas() {
        let script = ScriptBuilder::new("scripted", "scripted", "scripted-test")
            .with_chunk_size(2)
            .start()
            .text_block("hello")
            .done(DoneReason::Stop);

        // Drive the script through the provider to observe the event
        // sequence.
        let provider = ScriptedProvider::new(vec![script]);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        // Start + TextStart + 3 deltas ("he","ll","o") + TextEnd + Done = 7
        assert_eq!(events.len(), 7);

        // Each delta's partial should reflect the cumulative text.
        let snapshots: Vec<String> = events
            .iter()
            .filter_map(|ev| match ev {
                AssistantMessageEvent::TextDelta { partial, .. } => match partial.content.first() {
                    Some(AssistantContent::Text(t)) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(snapshots, vec!["he", "hell", "hello"]);
    }

    #[tokio::test]
    async fn builder_done_carries_partial_state() {
        let script = ScriptBuilder::new("scripted", "scripted", "scripted-test")
            .start()
            .text_block("hi")
            .done(DoneReason::Stop);
        let provider = ScriptedProvider::new(vec![script]);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        match events.last().expect("non-empty") {
            AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(*reason, DoneReason::Stop);
                assert_eq!(message.stop_reason, StopReason::Stop);
                assert_eq!(message.content.len(), 1);
                match &message.content[0] {
                    AssistantContent::Text(t) => assert_eq!(t.text, "hi"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn builder_error_carries_error_payload() {
        let script = ScriptBuilder::new("scripted", "scripted", "scripted-test")
            .start()
            .error(
                ErrorReason::Error,
                AssistantError::new(ErrorCategory::Transient, "scripted: synthetic"),
            );
        let provider = ScriptedProvider::new(vec![script]);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;
        match events.last().expect("non-empty") {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(*reason, ErrorReason::Error);
                assert_eq!(error.stop_reason, StopReason::Error);
                assert_eq!(error.error.as_ref().unwrap().message, "scripted: synthetic");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn provider_dispatches_per_script_per_inference() {
        // Two scripts: two separate `stream()` calls should each consume
        // one. The third call falls through to the exhausted-EndTurn path.
        let script_a = ScriptBuilder::new("scripted", "scripted", "scripted-test")
            .start()
            .text_block("one")
            .done(DoneReason::Stop);
        let script_b = ScriptBuilder::new("scripted", "scripted", "scripted-test")
            .start()
            .text_block("two")
            .done(DoneReason::Stop);
        let provider = ScriptedProvider::new(vec![script_a, script_b]);

        let s1 = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let s2 = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let s3 = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );

        let e1 = collect_events(s1).await;
        let e2 = collect_events(s2).await;
        let e3 = collect_events(s3).await;
        // First two scripts each emit Start + TextStart + Delta + TextEnd + Done = 5.
        assert_eq!(e1.len(), 5);
        assert_eq!(e2.len(), 5);
        // Third call exhausts the queue and falls back to a single Done.
        assert_eq!(e3.len(), 1);
        assert!(matches!(e3[0], AssistantMessageEvent::Done { .. }));
    }

    #[tokio::test]
    async fn stream_simple_dispatches_to_stream() {
        let provider = ScriptedProvider::from_event_vecs(vec![vec![AssistantMessageEvent::Done {
            reason: DoneReason::Stop,
            message: {
                let mut m = AssistantMessage::empty();
                m.api = "scripted".into();
                m.provider = "scripted".into();
                m.model = "scripted-test".into();
                m.stop_reason = StopReason::Stop;
                m
            },
        }]]);
        let opts = SimpleStreamOptions {
            base: StreamOptions::default(),
            reasoning: None,
        };
        let stream = provider.stream_simple(&fake_model(), &Context::new("system"), &opts);
        let events = collect_events(stream).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], AssistantMessageEvent::Done { .. }));
    }

    #[tokio::test]
    async fn builder_tool_call_block_chunks_serialized_arguments() {
        let arguments = serde_json::json!({"command": "echo hi"});
        let serialized = serde_json::to_string(&arguments).unwrap();
        let script = ScriptBuilder::new("scripted", "scripted", "scripted-test")
            .with_chunk_size(5)
            .start()
            .tool_call_block("tc-1", "bash", arguments.clone())
            .done(DoneReason::ToolUse);
        let provider = ScriptedProvider::new(vec![script]);
        let stream = provider.stream(
            &fake_model(),
            &Context::new("system"),
            &StreamOptions::default(),
        );
        let events = collect_events(stream).await;

        // The tool-call deltas should concatenate to the serialized JSON.
        let concatenated: String = events
            .iter()
            .filter_map(|ev| match ev {
                AssistantMessageEvent::ToolCallDelta { delta, .. } => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(concatenated, serialized);

        // The final partial inside ToolCallEnd should carry the parsed
        // arguments.
        let end = events
            .iter()
            .find(|ev| matches!(ev, AssistantMessageEvent::ToolCallEnd { .. }))
            .expect("ToolCallEnd present");
        match end {
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                assert_eq!(tool_call.arguments, arguments);
            }
            _ => unreachable!(),
        }
    }
}
