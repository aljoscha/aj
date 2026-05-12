//! Scripted [`Model`] implementation for tests, demos, and TUI eyeballing.
//!
//! `ScriptedModel` is a [`Model`] that replays canned [`StreamingEvent`]
//! sequences instead of calling out to a real LLM provider. It serves two
//! audiences:
//!
//! - **Tests.** The agent loop's snapshot tests build a script per inference
//!   and assert on the resulting [`AgentEvent`](aj_agent) sequence. They want
//!   strict behaviour: if the agent runs more inferences than scripted, the
//!   model panics so the regression is loud.
//! - **Demos / `--scripted` CLI mode.** The binary plugs `ScriptedModel`
//!   in place of the real provider so users can eyeball the TUI's rendering
//!   of thinking blocks, tool calls, errors, retries, etc., without a network
//!   round-trip. Demos run for an unknown number of turns (the user can chat
//!   freely after the canned script finishes), so the model defaults to a
//!   lenient end-of-turn fallback once the script is exhausted.
//!
//! Each scripted inference is a [`Script`] — a list of [`ScriptStep`]s where
//! each step carries an optional pre-emit delay. Delays let demo scripts
//! stream thinking and text progressively so the TUI's collapsible thinking
//! block, spinner, and live render path can be exercised visually. Tests
//! typically leave the delays at zero.
//!
//! A library of named demo scripts lives in [`demos`].
//!
//! # Future extensions
//!
//! Loading scripts from on-disk TOML/JSON would let users craft new repros
//! without recompiling. The in-process script vocabulary is stable enough to
//! grow a thin parser on top once the need shows up; that's deliberately not
//! implemented yet to keep this module small.

use std::pin::Pin;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;

use crate::messages::{ContentBlock, Message, MessageParam, MessageType, Role, StopReason, Usage};
use crate::streaming::StreamingEvent;
use crate::tools::Tool;
use crate::{Model, ModelError, ThinkingConfig};

pub mod demos;
pub mod provider;

/// A single streaming event with an optional pre-emit delay.
#[derive(Clone, Debug)]
pub struct ScriptStep {
    /// How long to sleep before emitting [`Self::event`].
    pub delay: Duration,
    pub event: StreamingEvent,
}

impl ScriptStep {
    pub fn new(delay: Duration, event: StreamingEvent) -> Self {
        Self { delay, event }
    }

    pub fn immediate(event: StreamingEvent) -> Self {
        Self {
            delay: Duration::ZERO,
            event,
        }
    }
}

/// One inference's worth of scripted events.
///
/// The agent issues one [`Model::run_inference_streaming`] call per loop
/// iteration. Each call consumes exactly one [`Script`] from the model's
/// queue and replays its [`ScriptStep`]s in order.
#[derive(Clone, Debug, Default)]
pub struct Script {
    pub steps: Vec<ScriptStep>,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a script from a flat list of [`StreamingEvent`]s with no
    /// inter-event delay. Convenient for snapshot tests where the timing
    /// is irrelevant.
    pub fn from_events(events: Vec<StreamingEvent>) -> Self {
        Self {
            steps: events.into_iter().map(ScriptStep::immediate).collect(),
        }
    }

    /// Append an event after a delay.
    pub fn push(mut self, delay: Duration, event: StreamingEvent) -> Self {
        self.steps.push(ScriptStep::new(delay, event));
        self
    }

    /// Append an event with no delay.
    pub fn push_immediate(mut self, event: StreamingEvent) -> Self {
        self.steps.push(ScriptStep::immediate(event));
        self
    }
}

/// What `ScriptedModel` does when the agent asks for more inferences than
/// the queued [`Script`]s supply.
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

/// A [`Model`] backed by a queue of [`Script`]s.
///
/// Cheap to construct; cheap to clone its events (every concrete
/// `StreamingEvent` we emit clones in O(payload) bytes). The script queue
/// lives behind a [`Mutex`] so the trait's `&self` receiver can dequeue
/// each call's script in turn.
pub struct ScriptedModel {
    scripts: Mutex<std::vec::IntoIter<Script>>,
    name: String,
    url: String,
    on_exhausted: ExhaustedBehavior,
}

impl ScriptedModel {
    /// Build a scripted model from a list of per-inference [`Script`]s.
    ///
    /// Defaults to [`ExhaustedBehavior::EndTurn`] (demo-friendly) and the
    /// `scripted` / `scripted://internal` identity pair. Use the builder
    /// methods to customise.
    pub fn new(scripts: Vec<Script>) -> Self {
        Self {
            scripts: Mutex::new(scripts.into_iter()),
            name: "scripted".to_string(),
            url: "scripted://internal".to_string(),
            on_exhausted: ExhaustedBehavior::EndTurn,
        }
    }

    /// Convenience: build a scripted model from a list of plain event vectors,
    /// no delays. Equivalent to `ScriptedModel::new(events.into_iter().map(Script::from_events).collect())`.
    pub fn from_event_vecs(scripts: Vec<Vec<StreamingEvent>>) -> Self {
        Self::new(scripts.into_iter().map(Script::from_events).collect())
    }

    pub fn on_exhausted(mut self, behavior: ExhaustedBehavior) -> Self {
        self.on_exhausted = behavior;
        self
    }

    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    pub fn with_url(mut self, url: impl Into<String>) -> Self {
        self.url = url.into();
        self
    }
}

#[async_trait]
impl Model for ScriptedModel {
    async fn run_inference_streaming(
        &self,
        _messages: &[MessageParam],
        _system_prompt: String,
        _tools: Vec<Tool>,
        _thinking: Option<ThinkingConfig>,
    ) -> Result<Pin<Box<dyn Stream<Item = StreamingEvent> + Send>>, ModelError> {
        let next = self.scripts.lock().unwrap().next();
        match next {
            Some(script) => {
                let stream = async_stream::stream! {
                    for step in script.steps {
                        if !step.delay.is_zero() {
                            tokio::time::sleep(step.delay).await;
                        }
                        yield step.event;
                    }
                };
                Ok(Box::pin(stream))
            }
            None => match self.on_exhausted {
                ExhaustedBehavior::Panic => {
                    panic!("ScriptedModel exhausted: agent ran more inferences than scripted");
                }
                ExhaustedBehavior::EndTurn => {
                    // Synthesize a minimal end-of-turn message so the agent
                    // loop exits cleanly. We don't emit any text/thinking
                    // stream events because they'd leak a stray "" into the
                    // renderer; the empty finalized message is enough for
                    // the agent to consider the turn complete.
                    let message = empty_end_turn_message(&self.name);
                    let stream = async_stream::stream! {
                        yield StreamingEvent::FinalizedMessage { message };
                    };
                    Ok(Box::pin(stream))
                }
            },
        }
    }

    fn model_name(&self) -> String {
        self.name.clone()
    }

    fn model_url(&self) -> String {
        self.url.clone()
    }
}

fn empty_end_turn_message(model: &str) -> Message {
    Message {
        id: "scripted-end".to_string(),
        r#type: MessageType::Message,
        role: Role::Assistant,
        content: Vec::new(),
        model: model.to_string(),
        stop_reason: Some(StopReason::EndTurn),
        stop_sequence: None,
        stop_details: None,
        usage: Usage::default(),
        container: None,
        context_management: None,
    }
}

// ===========================================================================
// Small helpers used by the demo library and by callers that want to build
// their own scripts without repeating boilerplate for every event variant.
// ===========================================================================

/// Build a finalized [`Message`] with the given content blocks and stop reason.
///
/// `model_name` shows up in agent-side logging and persistence; pick something
/// recognisable so a debugger can tell scripted runs apart from real ones.
pub fn finalized_message(
    model_name: &str,
    content: Vec<ContentBlock>,
    stop_reason: StopReason,
) -> Message {
    Message {
        id: format!("scripted-{}", model_name),
        r#type: MessageType::Message,
        role: Role::Assistant,
        content,
        model: model_name.to_string(),
        stop_reason: Some(stop_reason),
        stop_sequence: None,
        stop_details: None,
        usage: Usage::default(),
        container: None,
        context_management: None,
    }
}

/// Stream a text block in roughly `chunk_size`-character pieces.
///
/// Pushes `TextStart` (empty), one `TextUpdate` per chunk, and one final
/// `TextStop` with the accumulated text. The same shape a real provider
/// would emit for a text-only block.
pub fn stream_text_steps(text: &str, chunk_size: usize, per_chunk: Duration) -> Vec<ScriptStep> {
    let mut steps = Vec::new();
    steps.push(ScriptStep::immediate(StreamingEvent::TextStart {
        text: String::new(),
        citations: Vec::new(),
    }));

    let mut snapshot = String::new();
    for chunk in chunked(text, chunk_size) {
        snapshot.push_str(chunk);
        steps.push(ScriptStep::new(
            per_chunk,
            StreamingEvent::TextUpdate {
                diff: chunk.to_string(),
                snapshot: snapshot.clone(),
            },
        ));
    }

    steps.push(ScriptStep::immediate(StreamingEvent::TextStop {
        text: snapshot,
    }));
    steps
}

/// Stream a thinking block in roughly `chunk_size`-character pieces.
///
/// Symmetric to [`stream_text_steps`] but emits the thinking-channel
/// variants. The final `ThinkingStop` carries no payload; the renderer
/// treats it as a "thinking block finished" signal.
pub fn stream_thinking_steps(
    text: &str,
    chunk_size: usize,
    per_chunk: Duration,
) -> Vec<ScriptStep> {
    let mut steps = Vec::new();
    steps.push(ScriptStep::immediate(StreamingEvent::ThinkingStart {
        thinking: String::new(),
    }));

    let mut snapshot = String::new();
    for chunk in chunked(text, chunk_size) {
        snapshot.push_str(chunk);
        steps.push(ScriptStep::new(
            per_chunk,
            StreamingEvent::ThinkingUpdate {
                diff: chunk.to_string(),
                snapshot: snapshot.clone(),
            },
        ));
    }

    steps.push(ScriptStep::immediate(StreamingEvent::ThinkingStop));
    steps
}

/// Iterate `text` as `chunk_size`-grapheme-byte slices (best-effort: we split
/// on character boundaries, not graphemes, but never inside a multi-byte
/// UTF-8 scalar).
fn chunked(text: &str, chunk_size: usize) -> Vec<&str> {
    if chunk_size == 0 {
        return vec![text];
    }
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    while start < bytes.len() {
        // Walk `chunk_size` characters forward from `start`.
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn chunked_splits_at_char_boundaries_not_byte() {
        // Multi-byte UTF-8 scalars shouldn't be split mid-codepoint.
        let pieces = chunked("héllo", 2);
        assert!(
            pieces
                .iter()
                .all(|p| std::str::from_utf8(p.as_bytes()).is_ok())
        );
        assert_eq!(pieces.concat(), "héllo");
    }

    #[test]
    fn chunked_with_zero_returns_input_unchanged() {
        assert_eq!(chunked("hello", 0), vec!["hello"]);
    }

    #[tokio::test]
    async fn run_inference_replays_canned_events() {
        let script = Script::from_events(vec![
            StreamingEvent::TextStart {
                text: String::new(),
                citations: Vec::new(),
            },
            StreamingEvent::TextStop {
                text: "hi".to_string(),
            },
            StreamingEvent::FinalizedMessage {
                message: finalized_message(
                    "scripted",
                    vec![ContentBlock::new_text_block("hi".to_string())],
                    StopReason::EndTurn,
                ),
            },
        ]);
        let model = ScriptedModel::new(vec![script]);
        let stream = model
            .run_inference_streaming(&[], String::new(), Vec::new(), None)
            .await
            .expect("scripted stream");
        let events: Vec<StreamingEvent> = stream.collect().await;
        assert_eq!(events.len(), 3);
        assert!(matches!(events[0], StreamingEvent::TextStart { .. }));
        assert!(matches!(events[2], StreamingEvent::FinalizedMessage { .. }));
    }

    #[tokio::test]
    async fn exhausted_endturn_synthesizes_message() {
        let model = ScriptedModel::new(Vec::new());
        let stream = model
            .run_inference_streaming(&[], String::new(), Vec::new(), None)
            .await
            .expect("end-turn fallback");
        let events: Vec<StreamingEvent> = stream.collect().await;
        assert_eq!(events.len(), 1);
        match &events[0] {
            StreamingEvent::FinalizedMessage { message } => {
                assert!(matches!(message.stop_reason, Some(StopReason::EndTurn)));
                assert!(message.content.is_empty());
            }
            other => panic!("expected FinalizedMessage, got {other:?}"),
        }
    }

    #[tokio::test]
    #[should_panic(expected = "ScriptedModel exhausted")]
    async fn exhausted_panic_aborts_inference() {
        let model = ScriptedModel::new(Vec::new()).on_exhausted(ExhaustedBehavior::Panic);
        let _ = model
            .run_inference_streaming(&[], String::new(), Vec::new(), None)
            .await;
    }
}
