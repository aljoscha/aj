//! Internal compatibility shim: bridge legacy [`crate::Model`] impls onto
//! the unified [`crate::provider::Provider`] trait.
//!
//! [`LegacyProviderAdapter`] wraps an `Arc<dyn Model>` and produces an
//! [`AssistantMessageEventStream`] that mirrors the legacy
//! [`StreamingEvent`] sequence, synthesizing the per-event `partial`
//! snapshots the unified protocol requires.
//!
//! The adapter exists so the agent's inference loop can be flipped onto
//! the [`Provider`] trait before every call site (binary, scripted, sub-
//! agents) has migrated. By the time the migration completes, the legacy
//! [`Model`] trait and this module both go away. The module is
//! intentionally not re-exported from the crate root; the only consumer
//! is `aj-agent` during the transition.

use std::pin::pin;
use std::sync::Arc;

use futures::StreamExt;
use serde_json::Value;

use crate::Model;
use crate::ThinkingConfig;
use crate::messages::{
    ApiError, ContentBlock, ContentBlockParam, Message as LegacyMessage, Role,
    StopReason as LegacyStopReason, ToolResultContent, Usage as LegacyUsage,
};
use crate::provider::Provider;
use crate::registry::ModelInfo;
use crate::streaming::{
    AssistantMessageEvent, AssistantMessageEventStream, DoneReason, ErrorReason, StreamingEvent,
};
use crate::tools::Tool;
use crate::types::{
    AssistantContent, AssistantError, AssistantMessage, Context, ErrorCategory, Message,
    SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingContent, ThinkingLevel,
    ToolCall, ToolDefinition, ToolResultMessage, Usage, UserContent, UserMessage,
};

// ---------------------------------------------------------------------------
// Adapter
// ---------------------------------------------------------------------------

/// Adapts an `Arc<dyn Model>` to the unified [`Provider`] trait.
///
/// Each [`Provider::stream`] call projects the unified [`Context`] /
/// [`StreamOptions`] back onto the legacy [`Model::run_inference_streaming`]
/// shape, then drives the resulting [`StreamingEvent`] stream through a
/// transcoder that maintains an [`AssistantMessage`] partial and emits
/// unified [`AssistantMessageEvent`]s.
///
/// The wrapped [`Model`]'s `model_name()` / `model_url()` are ignored: the
/// adapter stamps `api` / `provider` / `model` from the [`ModelInfo`] the
/// caller supplies, matching how concrete providers behave.
#[derive(Clone)]
pub struct LegacyProviderAdapter {
    model: Arc<dyn Model>,
}

impl LegacyProviderAdapter {
    /// Wrap a legacy [`Model`] handle as a [`Provider`].
    pub fn new(model: Arc<dyn Model>) -> Self {
        Self { model }
    }

    /// Borrow the wrapped legacy handle. Used by `Agent::model()` accessors
    /// until step 6.7 drops the legacy constructor.
    pub fn inner(&self) -> &Arc<dyn Model> {
        &self.model
    }
}

impl Provider for LegacyProviderAdapter {
    fn stream(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        self.stream_with_thinking(model, context, options, None)
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        let thinking = options.reasoning.as_ref().map(map_thinking_level_to_legacy);
        self.stream_with_thinking(model, context, &options.base, thinking)
    }
}

impl LegacyProviderAdapter {
    /// Shared implementation for `stream` / `stream_simple`: project the
    /// unified context to legacy inputs, spawn a transcoder task, return
    /// the consumer-side stream handle.
    fn stream_with_thinking(
        &self,
        model: &ModelInfo,
        context: &Context,
        _options: &StreamOptions,
        thinking: Option<ThinkingConfig>,
    ) -> AssistantMessageEventStream {
        let stream = AssistantMessageEventStream::new();
        let producer = stream.clone();

        let messages = project_messages_to_legacy(&context.messages);
        let system_prompt = context.system_prompt.clone().unwrap_or_default();
        let tools = project_tools_to_legacy(&context.tools);
        let model_for_task = Arc::clone(&self.model);
        let identity = ProviderIdentity {
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
        };

        // The legacy `run_inference_streaming` is async and returns an owned
        // stream; we drive it from a spawned task so [`Provider::stream`]
        // can return synchronously like the real provider impls do.
        tokio::spawn(async move {
            run_transcoder(
                model_for_task,
                producer,
                identity,
                messages,
                system_prompt,
                tools,
                thinking,
            )
            .await;
        });

        stream
    }
}

// ---------------------------------------------------------------------------
// Transcoder driver
// ---------------------------------------------------------------------------

/// Identity fields stamped onto every emitted partial / terminal message.
#[derive(Clone)]
struct ProviderIdentity {
    api: String,
    provider: String,
    model: String,
}

async fn run_transcoder(
    model: Arc<dyn Model>,
    producer: AssistantMessageEventStream,
    identity: ProviderIdentity,
    messages: Vec<crate::messages::MessageParam>,
    system_prompt: String,
    tools: Vec<Tool>,
    thinking: Option<ThinkingConfig>,
) {
    // Kick off the legacy stream; surface a setup-time error as an
    // immediate terminal `Error` event so the consumer always sees a
    // well-formed stream.
    let legacy_stream = match model
        .run_inference_streaming(&messages, system_prompt, tools, thinking)
        .await
    {
        Ok(s) => s,
        Err(err) => {
            let mut error = empty_message(&identity);
            error.stop_reason = StopReason::Error;
            error.error = Some(AssistantError::new(
                ErrorCategory::Transient,
                err.to_string(),
            ));
            producer.push(AssistantMessageEvent::Error {
                reason: ErrorReason::Error,
                error,
            });
            return;
        }
    };

    let mut transcoder = Transcoder::new(identity.clone());
    let mut terminal_emitted = false;
    {
        let mut legacy_stream = pin!(legacy_stream);
        while let Some(event) = legacy_stream.next().await {
            // Once the transcoder emits a terminal event we stop draining
            // — further legacy events would be dropped by
            // `AssistantMessageEventStream::push` anyway, but exiting the
            // loop frees up the upstream HTTP body sooner.
            transcoder.handle(event, &producer);
            if transcoder.terminated {
                terminal_emitted = true;
                break;
            }
        }
    }

    if !terminal_emitted {
        // The legacy stream closed without emitting `FinalizedMessage` or
        // `Error`. Synthesize a transient error so consumers awaiting
        // `result()` always see a typed terminal event.
        let mut error = transcoder.partial.clone();
        error.stop_reason = StopReason::Error;
        error.error = Some(AssistantError::new(
            ErrorCategory::Transient,
            "legacy provider stream ended without a terminal event",
        ));
        producer.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error,
        });
    }
}

/// Walks legacy [`StreamingEvent`]s and emits unified
/// [`AssistantMessageEvent`]s, maintaining the per-event `partial`
/// snapshot the consumer expects.
struct Transcoder {
    identity: ProviderIdentity,
    /// Running snapshot cloned into each emitted event.
    partial: AssistantMessage,
    /// Cumulative legacy usage; mapped onto `partial.usage` on every
    /// update so consumers always see a coherent unified usage snapshot.
    legacy_usage: LegacyUsage,
    /// Index of the currently open content block (`None` between blocks).
    current_index: Option<usize>,
    /// True once we've emitted the leading `Start` event for this stream.
    started: bool,
    /// True once we've emitted a terminal `Done` or `Error`.
    terminated: bool,
}

impl Transcoder {
    fn new(identity: ProviderIdentity) -> Self {
        let partial = empty_message(&identity);
        Self {
            identity,
            partial,
            legacy_usage: LegacyUsage::default(),
            current_index: None,
            started: false,
            terminated: false,
        }
    }

    fn ensure_started(&mut self, producer: &AssistantMessageEventStream) {
        if self.started {
            return;
        }
        self.started = true;
        producer.push(AssistantMessageEvent::Start {
            partial: self.partial.clone(),
        });
    }

    fn handle(&mut self, event: StreamingEvent, producer: &AssistantMessageEventStream) {
        match event {
            StreamingEvent::MessageStart { message } => {
                // Legacy providers carry the resolved model id here. We
                // already have authoritative identity from `ModelInfo`, so
                // we only seed the response id if the legacy message
                // surfaced one.
                if !message.id.is_empty() {
                    self.partial.response_id = Some(message.id);
                }
                // Pre-seed `partial.usage` if the start event already
                // carries usage (Anthropic does on message_start).
                self.legacy_usage = message.usage;
                self.partial.usage = map_usage_to_unified(&self.legacy_usage);
                self.ensure_started(producer);
            }
            StreamingEvent::UsageUpdate { usage } => {
                self.legacy_usage.apply_delta(&usage);
                self.partial.usage = map_usage_to_unified(&self.legacy_usage);
                // Per spec: no separate event — usage rides on subsequent
                // partial snapshots.
            }
            StreamingEvent::TextStart { text, citations: _ } => {
                self.ensure_started(producer);
                let idx = self.partial.content.len();
                self.partial
                    .content
                    .push(AssistantContent::Text(TextContent {
                        text: text.clone(),
                        text_signature: None,
                    }));
                self.current_index = Some(idx);
                producer.push(AssistantMessageEvent::TextStart {
                    content_index: idx,
                    partial: self.partial.clone(),
                });
                if !text.is_empty() {
                    producer.push(AssistantMessageEvent::TextDelta {
                        content_index: idx,
                        delta: text,
                        partial: self.partial.clone(),
                    });
                }
            }
            StreamingEvent::TextUpdate { diff, snapshot: _ } => {
                let Some(idx) = self.current_index else {
                    // Out-of-order delta without a matching Start; drop
                    // it rather than synthesise an extra block — the
                    // legacy providers shouldn't emit this shape but
                    // we defend against malformed scripts.
                    return;
                };
                if let Some(AssistantContent::Text(t)) = self.partial.content.get_mut(idx) {
                    t.text.push_str(&diff);
                }
                producer.push(AssistantMessageEvent::TextDelta {
                    content_index: idx,
                    delta: diff,
                    partial: self.partial.clone(),
                });
            }
            StreamingEvent::TextStop { text } => {
                let Some(idx) = self.current_index.take() else {
                    return;
                };
                // Reconcile against the provider's authoritative final
                // text — legacy providers occasionally emit a stop with
                // the complete content even when deltas got dropped.
                if let Some(AssistantContent::Text(t)) = self.partial.content.get_mut(idx) {
                    if !text.is_empty() {
                        t.text = text.clone();
                    }
                }
                let content = match self.partial.content.get(idx) {
                    Some(AssistantContent::Text(t)) => t.text.clone(),
                    _ => text,
                };
                producer.push(AssistantMessageEvent::TextEnd {
                    content_index: idx,
                    content,
                    partial: self.partial.clone(),
                });
            }
            StreamingEvent::ThinkingStart { thinking } => {
                self.ensure_started(producer);
                let idx = self.partial.content.len();
                self.partial
                    .content
                    .push(AssistantContent::Thinking(ThinkingContent {
                        thinking: thinking.clone(),
                        thinking_signature: None,
                        redacted: false,
                    }));
                self.current_index = Some(idx);
                producer.push(AssistantMessageEvent::ThinkingStart {
                    content_index: idx,
                    partial: self.partial.clone(),
                });
                if !thinking.is_empty() {
                    producer.push(AssistantMessageEvent::ThinkingDelta {
                        content_index: idx,
                        delta: thinking,
                        partial: self.partial.clone(),
                    });
                }
            }
            StreamingEvent::ThinkingUpdate { diff, snapshot: _ } => {
                let Some(idx) = self.current_index else {
                    return;
                };
                if let Some(AssistantContent::Thinking(t)) = self.partial.content.get_mut(idx) {
                    t.thinking.push_str(&diff);
                }
                producer.push(AssistantMessageEvent::ThinkingDelta {
                    content_index: idx,
                    delta: diff,
                    partial: self.partial.clone(),
                });
            }
            StreamingEvent::ThinkingStop => {
                let Some(idx) = self.current_index.take() else {
                    return;
                };
                let content = match self.partial.content.get(idx) {
                    Some(AssistantContent::Thinking(t)) => t.thinking.clone(),
                    _ => String::new(),
                };
                producer.push(AssistantMessageEvent::ThinkingEnd {
                    content_index: idx,
                    content,
                    partial: self.partial.clone(),
                });
            }
            StreamingEvent::FinalizedMessage { message } => {
                self.ensure_started(producer);
                self.emit_finalized(message, producer);
                self.terminated = true;
            }
            StreamingEvent::Error { error } => {
                self.ensure_started(producer);
                let mut err_msg = self.partial.clone();
                err_msg.stop_reason = StopReason::Error;
                err_msg.error = Some(classify_legacy_api_error(&error));
                producer.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: err_msg,
                });
                self.terminated = true;
            }
            StreamingEvent::ParseError { error, raw_data } => {
                // Non-fatal in the legacy world, but the unified
                // protocol only has terminal `Error`. Surface as
                // Transient so the agent treats it as retryable.
                self.ensure_started(producer);
                let mut err_msg = self.partial.clone();
                err_msg.stop_reason = StopReason::Error;
                err_msg.error = Some(AssistantError::new(
                    ErrorCategory::Transient,
                    format!("legacy stream parse error: {error} (raw: {raw_data})"),
                ));
                producer.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: err_msg,
                });
                self.terminated = true;
            }
            StreamingEvent::ProtocolError { error } => {
                self.ensure_started(producer);
                let mut err_msg = self.partial.clone();
                err_msg.stop_reason = StopReason::Error;
                err_msg.error = Some(AssistantError::new(
                    ErrorCategory::Transient,
                    format!("legacy protocol error: {error}"),
                ));
                producer.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: err_msg,
                });
                self.terminated = true;
            }
            StreamingEvent::ToolUseParseError {
                id,
                name,
                error,
                raw_data: _,
            } => {
                // The legacy agent's behaviour was: emit a diagnostic,
                // resurrect the tool_use block with `null` input, and let
                // the per-tool validation surface an `is_error` tool_result.
                // We mirror that through the unified protocol by emitting
                // a real tool-call block with `Value::Null` arguments; the
                // agent's execution loop rejects it the same way.
                //
                // The error string is folded into the partial's response_id
                // as a debug hint (it never round-trips through the wire)
                // — the agent's new `Notice` path will replace this when
                // step 6.5 lands. For now the synthesized tool_use is
                // enough to drive the existing recovery path.
                self.ensure_started(producer);
                let _ = error;
                let idx = self.partial.content.len();
                let tc = ToolCall {
                    id,
                    name,
                    arguments: Value::Null,
                };
                self.partial
                    .content
                    .push(AssistantContent::ToolCall(tc.clone()));
                producer.push(AssistantMessageEvent::ToolCallStart {
                    content_index: idx,
                    partial: self.partial.clone(),
                });
                producer.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: idx,
                    tool_call: tc,
                    partial: self.partial.clone(),
                });
            }
        }
    }

    /// Translate the legacy `FinalizedMessage` into a unified
    /// `AssistantMessageEvent::Done`. Synthesises `ToolCallStart` /
    /// `ToolCallEnd` events for any tool_use blocks that didn't surface
    /// via streaming (the legacy protocol has no tool_use streaming, so
    /// every tool call arrives here).
    fn emit_finalized(&mut self, message: LegacyMessage, producer: &AssistantMessageEventStream) {
        // Capture usage off the finalized message — providers stamp the
        // canonical usage here even when streaming sent partial deltas.
        self.legacy_usage = message.usage.clone();
        self.partial.usage = map_usage_to_unified(&self.legacy_usage);
        if !message.id.is_empty() {
            self.partial.response_id = Some(message.id.clone());
        }

        // Walk the finalized blocks and synthesise stream events for any
        // tool_use that the streaming layer didn't pre-announce. We don't
        // re-emit text/thinking blocks that were already streamed.
        for block in &message.content {
            if let ContentBlock::ToolUseBlock {
                id,
                input,
                name,
                caller: _,
            } = block
            {
                let idx = self.partial.content.len();
                let tc = ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                };
                self.partial
                    .content
                    .push(AssistantContent::ToolCall(tc.clone()));
                producer.push(AssistantMessageEvent::ToolCallStart {
                    content_index: idx,
                    partial: self.partial.clone(),
                });
                producer.push(AssistantMessageEvent::ToolCallEnd {
                    content_index: idx,
                    tool_call: tc,
                    partial: self.partial.clone(),
                });
            }
        }

        // Rebuild the final message from its actual content (preferring
        // the legacy finalized blocks over the streamed partial), so the
        // `Done` event carries the provider's authoritative shape.
        let mut final_message = AssistantMessage {
            content: message
                .content
                .iter()
                .filter_map(map_legacy_block_to_assistant)
                .collect(),
            api: self.identity.api.clone(),
            provider: self.identity.provider.clone(),
            model: self.identity.model.clone(),
            response_id: self.partial.response_id.clone(),
            usage: self.partial.usage.clone(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        };
        let (unified_stop, done_reason) = map_legacy_stop_reason(message.stop_reason);
        final_message.stop_reason = unified_stop;

        producer.push(AssistantMessageEvent::Done {
            reason: done_reason,
            message: final_message,
        });
    }
}

// ---------------------------------------------------------------------------
// Projection helpers: unified → legacy
// ---------------------------------------------------------------------------

fn project_messages_to_legacy(messages: &[Message]) -> Vec<crate::messages::MessageParam> {
    messages.iter().map(project_message_to_legacy).collect()
}

fn project_message_to_legacy(message: &Message) -> crate::messages::MessageParam {
    match message {
        Message::User(u) => project_user_message(u),
        Message::Assistant(a) => project_assistant_message(a),
        Message::ToolResult(t) => project_tool_result_message(t),
    }
}

fn project_user_message(u: &UserMessage) -> crate::messages::MessageParam {
    let content = u.content.iter().map(project_user_content).collect();
    crate::messages::MessageParam {
        role: Role::User,
        content,
    }
}

fn project_user_content(c: &UserContent) -> ContentBlockParam {
    match c {
        UserContent::Text(t) => ContentBlockParam::TextBlock {
            text: t.text.clone(),
            citations: None,
            signature: t.text_signature.clone(),
        },
        UserContent::Image(img) => ContentBlockParam::ImageBlock {
            source: crate::messages::ImageSource::Base64 {
                data: img.data.clone(),
                media_type: img.mime_type.clone(),
            },
        },
    }
}

fn project_assistant_message(a: &AssistantMessage) -> crate::messages::MessageParam {
    let content = a.content.iter().map(project_assistant_content).collect();
    crate::messages::MessageParam {
        role: Role::Assistant,
        content,
    }
}

fn project_assistant_content(c: &AssistantContent) -> ContentBlockParam {
    match c {
        AssistantContent::Text(t) => ContentBlockParam::TextBlock {
            text: t.text.clone(),
            citations: None,
            signature: t.text_signature.clone(),
        },
        AssistantContent::Thinking(t) => {
            if t.redacted {
                ContentBlockParam::RedactedThinkingBlock {
                    data: t.thinking_signature.clone().unwrap_or_default(),
                }
            } else {
                ContentBlockParam::ThinkingBlock {
                    signature: t.thinking_signature.clone().unwrap_or_default(),
                    thinking: t.thinking.clone(),
                }
            }
        }
        AssistantContent::ToolCall(tc) => ContentBlockParam::ToolUseBlock {
            id: tc.id.clone(),
            input: tc.arguments.clone(),
            name: tc.name.clone(),
            caller: None,
        },
    }
}

fn project_tool_result_message(t: &ToolResultMessage) -> crate::messages::MessageParam {
    // The legacy wire wraps tool results inside a user-role message
    // carrying a single ToolResultBlock. Multiple unified
    // `UserContent` blocks (e.g. text + image) collapse into the
    // block's `content` field via `ToolResultContent::Blocks`.
    let blocks: Vec<ContentBlockParam> = t.content.iter().map(project_user_content).collect();
    let content = ContentBlockParam::ToolResultBlock {
        tool_use_id: t.tool_call_id.clone(),
        content: ToolResultContent::Blocks(blocks),
        is_error: t.is_error,
    };
    crate::messages::MessageParam {
        role: Role::User,
        content: vec![content],
    }
}

fn project_tools_to_legacy(tools: &[ToolDefinition]) -> Vec<Tool> {
    tools
        .iter()
        .map(|t| Tool {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.parameters.clone(),
            r#type: None,
        })
        .collect()
}

fn map_thinking_level_to_legacy(level: &ThinkingLevel) -> ThinkingConfig {
    match level {
        // Legacy `ThinkingConfig` has no `Minimal` rung; map to the
        // smallest available bucket so providers that genuinely care
        // (Anthropic legacy) still enable thinking.
        ThinkingLevel::Minimal | ThinkingLevel::Low => ThinkingConfig::Low,
        ThinkingLevel::Medium => ThinkingConfig::Medium,
        ThinkingLevel::High => ThinkingConfig::High,
        ThinkingLevel::XHigh => ThinkingConfig::XHigh,
    }
}

// ---------------------------------------------------------------------------
// Projection helpers: legacy → unified
// ---------------------------------------------------------------------------

fn map_legacy_block_to_assistant(block: &ContentBlock) -> Option<AssistantContent> {
    match block {
        ContentBlock::TextBlock {
            text,
            citations: _,
            signature,
        } => Some(AssistantContent::Text(TextContent {
            text: text.clone(),
            text_signature: signature.clone(),
        })),
        ContentBlock::ThinkingBlock {
            signature,
            thinking,
        } => Some(AssistantContent::Thinking(ThinkingContent {
            thinking: thinking.clone(),
            thinking_signature: Some(signature.clone()),
            redacted: false,
        })),
        ContentBlock::RedactedThinkingBlock { data } => {
            Some(AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some(data.clone()),
                redacted: true,
            }))
        }
        ContentBlock::ToolUseBlock {
            id,
            input,
            name,
            caller: _,
        } => Some(AssistantContent::ToolCall(ToolCall {
            id: id.clone(),
            name: name.clone(),
            arguments: input.clone(),
        })),
        // Server-side tool plumbing, MCP blocks, container uploads, etc.
        // never appear on inference output for local tools so we drop
        // them; if they ever do, a future variant on AssistantContent
        // can route them through.
        _ => None,
    }
}

fn map_usage_to_unified(legacy: &LegacyUsage) -> Usage {
    let input = legacy.input_tokens;
    let output = legacy.output_tokens;
    let cache_read = legacy.cache_read_input_tokens.unwrap_or(0);
    let cache_write = legacy.cache_creation_input_tokens.unwrap_or(0);
    Usage {
        input,
        output,
        cache_read,
        cache_write,
        total_tokens: input + output + cache_read + cache_write,
        cost: crate::types::UsageCost::default(),
    }
}

fn map_legacy_stop_reason(reason: Option<LegacyStopReason>) -> (StopReason, DoneReason) {
    match reason {
        Some(LegacyStopReason::EndTurn)
        | Some(LegacyStopReason::StopSequence)
        | Some(LegacyStopReason::PauseTurn)
        | Some(LegacyStopReason::Compaction)
        | None => (StopReason::Stop, DoneReason::Stop),
        Some(LegacyStopReason::MaxTokens) => (StopReason::Length, DoneReason::Length),
        Some(LegacyStopReason::ToolUse) => (StopReason::ToolUse, DoneReason::ToolUse),
        // Refusal / context-window-exceeded technically aren't "successful"
        // terminations, but the legacy agent surfaces them through the
        // normal finalized-message path. We preserve that by emitting
        // `Done { reason: Stop }`; downstream code that cares about
        // either case can inspect the message's content blocks.
        Some(LegacyStopReason::Refusal) | Some(LegacyStopReason::ModelContextWindowExceeded) => {
            (StopReason::Stop, DoneReason::Stop)
        }
    }
}

fn classify_legacy_api_error(err: &ApiError) -> AssistantError {
    let (category, message) = match err {
        ApiError::AuthenticationError { message } => (ErrorCategory::Auth, message.clone()),
        ApiError::PermissionError { message } => (ErrorCategory::Auth, message.clone()),
        ApiError::RateLimitError { message } => (ErrorCategory::RateLimit, message.clone()),
        ApiError::OverloadedError { message } => (ErrorCategory::Overloaded, message.clone()),
        ApiError::GatewayTimeoutError { message } => (ErrorCategory::Transient, message.clone()),
        ApiError::InvalidRequestError { message } => {
            // Legacy paths never split context-overflow off invalid
            // requests at the wire layer — `is_context_overflow`
            // inspects the message after the fact. Surface as
            // InvalidRequest; callers can re-classify if needed.
            (ErrorCategory::InvalidRequest, message.clone())
        }
        ApiError::NotFoundError { message } => (ErrorCategory::InvalidRequest, message.clone()),
        ApiError::BillingError { message } => (ErrorCategory::InvalidRequest, message.clone()),
        ApiError::ApiError { message } => (ErrorCategory::Unknown, message.clone()),
    };
    AssistantError::new(category, message)
}

fn empty_message(identity: &ProviderIdentity) -> AssistantMessage {
    let mut msg = AssistantMessage::empty();
    msg.api = identity.api.clone();
    msg.provider = identity.provider.clone();
    msg.model = identity.model.clone();
    msg
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use crate::messages::{
        Caller, ContentBlock as LegacyContentBlock, Message as LegacyMessage, MessageType,
        Role as LegacyRole,
    };
    use crate::registry::{InputModality, ModelCost};
    use crate::scripted::{ExhaustedBehavior, Script, ScriptedModel};
    use crate::streaming::StreamingEvent;
    use crate::types::{Context, SimpleStreamOptions, StreamOptions, ThinkingLevel};

    use futures::StreamExt;

    fn fake_model_info() -> ModelInfo {
        ModelInfo {
            id: "legacy-test-model".into(),
            name: "Legacy Test".into(),
            api: "legacy-test".into(),
            provider: "legacy-test-provider".into(),
            base_url: "legacy://internal".into(),
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

    fn legacy_message_with_blocks(
        blocks: Vec<LegacyContentBlock>,
        stop: Option<LegacyStopReason>,
    ) -> LegacyMessage {
        LegacyMessage {
            id: "msg_legacy_1".into(),
            r#type: MessageType::Message,
            role: LegacyRole::Assistant,
            content: blocks,
            model: "legacy-test-model".into(),
            stop_reason: stop,
            stop_sequence: None,
            stop_details: None,
            usage: LegacyUsage {
                input_tokens: 7,
                output_tokens: 11,
                cache_read_input_tokens: Some(3),
                cache_creation_input_tokens: Some(2),
                ..LegacyUsage::default()
            },
            container: None,
            context_management: None,
        }
    }

    async fn drain(mut stream: AssistantMessageEventStream) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push(ev);
        }
        events
    }

    /// The adapter converts a legacy `FinalizedMessage` with one text
    /// block into the unified `Start` → `Done` sequence, stamping the
    /// `ModelInfo` identity fields onto the partial and final message.
    #[tokio::test]
    async fn finalized_text_only_message_emits_start_and_done() {
        let legacy_msg = legacy_message_with_blocks(
            vec![LegacyContentBlock::TextBlock {
                text: "hello world".into(),
                citations: Vec::new(),
                signature: None,
            }],
            Some(LegacyStopReason::EndTurn),
        );
        let script = Script::from_events(vec![StreamingEvent::FinalizedMessage {
            message: legacy_msg,
        }]);
        let model: Arc<dyn Model> =
            Arc::new(ScriptedModel::new(vec![script]).on_exhausted(ExhaustedBehavior::Panic));
        let adapter = LegacyProviderAdapter::new(model);
        let info = fake_model_info();
        let stream = adapter.stream(&info, &Context::new(""), &StreamOptions::default());

        let events = drain(stream).await;

        // Start, Done — synthesised in that order.
        assert!(matches!(
            events.first(),
            Some(AssistantMessageEvent::Start { .. })
        ));
        let last = events.last().expect("terminal event");
        let AssistantMessageEvent::Done { reason, message } = last else {
            panic!("expected Done, got {last:?}");
        };
        assert_eq!(*reason, DoneReason::Stop);
        assert_eq!(message.api, "legacy-test");
        assert_eq!(message.provider, "legacy-test-provider");
        assert_eq!(message.model, "legacy-test-model");
        assert_eq!(message.stop_reason, StopReason::Stop);
        assert_eq!(message.content.len(), 1);
        match &message.content[0] {
            AssistantContent::Text(t) => assert_eq!(t.text, "hello world"),
            other => panic!("expected text, got {other:?}"),
        }
        // Usage is projected from the legacy struct.
        assert_eq!(message.usage.input, 7);
        assert_eq!(message.usage.output, 11);
        assert_eq!(message.usage.cache_read, 3);
        assert_eq!(message.usage.cache_write, 2);
        assert_eq!(message.usage.total_tokens, 7 + 11 + 3 + 2);
        // Response id rides over from the legacy message id.
        assert_eq!(message.response_id.as_deref(), Some("msg_legacy_1"));
    }

    /// A legacy `FinalizedMessage` containing a tool_use block produces
    /// synthesised `ToolCallStart` / `ToolCallEnd` events (the legacy
    /// protocol has no tool_use streaming), and the terminal `Done`
    /// reports `DoneReason::ToolUse`.
    #[tokio::test]
    async fn finalized_tool_use_synthesises_tool_call_events() {
        let legacy_msg = legacy_message_with_blocks(
            vec![LegacyContentBlock::ToolUseBlock {
                id: "tu-1".into(),
                name: "ping".into(),
                input: serde_json::json!({"foo": "bar"}),
                caller: None,
            }],
            Some(LegacyStopReason::ToolUse),
        );
        let script = Script::from_events(vec![StreamingEvent::FinalizedMessage {
            message: legacy_msg,
        }]);
        let model: Arc<dyn Model> =
            Arc::new(ScriptedModel::new(vec![script]).on_exhausted(ExhaustedBehavior::Panic));
        let adapter = LegacyProviderAdapter::new(model);
        let info = fake_model_info();
        let stream = adapter.stream(&info, &Context::new(""), &StreamOptions::default());
        let events = drain(stream).await;

        // Expect: Start, ToolCallStart, ToolCallEnd, Done.
        let kinds: Vec<&'static str> = events
            .iter()
            .map(|e| match e {
                AssistantMessageEvent::Start { .. } => "Start",
                AssistantMessageEvent::ToolCallStart { .. } => "ToolCallStart",
                AssistantMessageEvent::ToolCallEnd { .. } => "ToolCallEnd",
                AssistantMessageEvent::Done { .. } => "Done",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, vec!["Start", "ToolCallStart", "ToolCallEnd", "Done"]);
        let last = events.last().expect("terminal");
        match last {
            AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(*reason, DoneReason::ToolUse);
                assert_eq!(message.stop_reason, StopReason::ToolUse);
                assert_eq!(message.content.len(), 1);
                match &message.content[0] {
                    AssistantContent::ToolCall(tc) => {
                        assert_eq!(tc.id, "tu-1");
                        assert_eq!(tc.name, "ping");
                        assert_eq!(tc.arguments["foo"], "bar");
                    }
                    other => panic!("expected ToolCall, got {other:?}"),
                }
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// Streaming text deltas project onto `TextStart` / `TextDelta` /
    /// `TextEnd`, and the partial's running text reflects the diffs.
    #[tokio::test]
    async fn streaming_text_deltas_round_trip() {
        let script = Script::from_events(vec![
            StreamingEvent::TextStart {
                text: "".into(),
                citations: Vec::new(),
            },
            StreamingEvent::TextUpdate {
                diff: "hel".into(),
                snapshot: "hel".into(),
            },
            StreamingEvent::TextUpdate {
                diff: "lo".into(),
                snapshot: "hello".into(),
            },
            StreamingEvent::TextStop {
                text: "hello".into(),
            },
            StreamingEvent::FinalizedMessage {
                message: legacy_message_with_blocks(
                    vec![LegacyContentBlock::TextBlock {
                        text: "hello".into(),
                        citations: Vec::new(),
                        signature: None,
                    }],
                    Some(LegacyStopReason::EndTurn),
                ),
            },
        ]);
        let model: Arc<dyn Model> =
            Arc::new(ScriptedModel::new(vec![script]).on_exhausted(ExhaustedBehavior::Panic));
        let adapter = LegacyProviderAdapter::new(model);
        let info = fake_model_info();
        let stream = adapter.stream(&info, &Context::new(""), &StreamOptions::default());
        let events = drain(stream).await;

        // Verify the partial in the final delta grew the expected text.
        let last_delta = events
            .iter()
            .rev()
            .find_map(|e| match e {
                AssistantMessageEvent::TextDelta { partial, delta, .. } => {
                    Some((partial.clone(), delta.clone()))
                }
                _ => None,
            })
            .expect("at least one TextDelta");
        assert_eq!(last_delta.1, "lo");
        match &last_delta.0.content[0] {
            AssistantContent::Text(t) => assert_eq!(t.text, "hello"),
            _ => panic!("expected text content"),
        }

        // The terminal `Done` carries the reconciled text.
        let terminal = events.last().expect("terminal");
        match terminal {
            AssistantMessageEvent::Done { message, .. } => match &message.content[0] {
                AssistantContent::Text(t) => assert_eq!(t.text, "hello"),
                _ => panic!("expected text"),
            },
            other => panic!("expected Done, got {other:?}"),
        }
    }

    /// A legacy `Error` event with a recognized variant maps to a
    /// terminal unified `Error` carrying the right [`ErrorCategory`].
    #[tokio::test]
    async fn legacy_api_error_maps_to_unified_error() {
        let script = Script::from_events(vec![StreamingEvent::Error {
            error: ApiError::OverloadedError {
                message: "overloaded".into(),
            },
        }]);
        let model: Arc<dyn Model> =
            Arc::new(ScriptedModel::new(vec![script]).on_exhausted(ExhaustedBehavior::Panic));
        let adapter = LegacyProviderAdapter::new(model);
        let info = fake_model_info();
        let stream = adapter.stream(&info, &Context::new(""), &StreamOptions::default());
        let events = drain(stream).await;

        let terminal = events.last().expect("terminal");
        match terminal {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(*reason, ErrorReason::Error);
                assert_eq!(error.stop_reason, StopReason::Error);
                let err = error.error.as_ref().expect("error payload");
                assert_eq!(err.category, ErrorCategory::Overloaded);
                assert!(err.message.contains("overloaded"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// `ToolUseParseError` synthesises a tool call carrying `Value::Null`
    /// arguments, so the agent's per-tool input-schema validation can
    /// reject it on the existing recovery path.
    #[tokio::test]
    async fn tool_use_parse_error_synthesises_null_tool_call() {
        let script = Script::from_events(vec![
            StreamingEvent::ToolUseParseError {
                id: "tu-bad".into(),
                name: "ping".into(),
                error: "bad json".into(),
                raw_data: "{".into(),
            },
            StreamingEvent::FinalizedMessage {
                message: legacy_message_with_blocks(Vec::new(), Some(LegacyStopReason::EndTurn)),
            },
        ]);
        let model: Arc<dyn Model> =
            Arc::new(ScriptedModel::new(vec![script]).on_exhausted(ExhaustedBehavior::Panic));
        let adapter = LegacyProviderAdapter::new(model);
        let info = fake_model_info();
        let stream = adapter.stream(&info, &Context::new(""), &StreamOptions::default());
        let events = drain(stream).await;

        let tc = events
            .iter()
            .find_map(|e| match e {
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => Some(tool_call.clone()),
                _ => None,
            })
            .expect("at least one tool_call_end");
        assert_eq!(tc.id, "tu-bad");
        assert_eq!(tc.name, "ping");
        assert!(tc.arguments.is_null());
    }

    /// Stream that ends with no terminal `FinalizedMessage` or `Error`
    /// surfaces a synthesised transient terminal error so consumers
    /// awaiting `result()` always see a typed event.
    #[tokio::test]
    async fn unterminated_legacy_stream_synthesises_transient_error() {
        let script = Script::from_events(vec![StreamingEvent::TextStart {
            text: "partial".into(),
            citations: Vec::new(),
        }]);
        let model: Arc<dyn Model> =
            Arc::new(ScriptedModel::new(vec![script]).on_exhausted(ExhaustedBehavior::Panic));
        let adapter = LegacyProviderAdapter::new(model);
        let info = fake_model_info();
        let stream = adapter.stream(&info, &Context::new(""), &StreamOptions::default());
        let events = drain(stream).await;

        let terminal = events.last().expect("terminal");
        match terminal {
            AssistantMessageEvent::Error { reason, error } => {
                assert_eq!(*reason, ErrorReason::Error);
                let err = error.error.as_ref().expect("error payload");
                assert_eq!(err.category, ErrorCategory::Transient);
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// `stream_simple`'s reasoning gets translated to the legacy
    /// `ThinkingConfig` enum so the wrapped model sees the requested
    /// thinking depth.
    #[tokio::test]
    async fn stream_simple_passes_thinking_to_legacy_model() {
        use std::pin::Pin;
        use std::sync::Arc as StdArc;
        use std::sync::Mutex as StdMutex;

        // Custom model that records what thinking config it sees.
        struct RecordingModel {
            seen: StdArc<StdMutex<Option<ThinkingConfig>>>,
        }

        #[async_trait::async_trait]
        impl Model for RecordingModel {
            async fn run_inference_streaming(
                &self,
                _messages: &[crate::messages::MessageParam],
                _system_prompt: String,
                _tools: Vec<Tool>,
                thinking: Option<ThinkingConfig>,
            ) -> Result<
                Pin<Box<dyn futures::Stream<Item = StreamingEvent> + Send>>,
                crate::ModelError,
            > {
                *self.seen.lock().unwrap() = thinking;
                let events = vec![StreamingEvent::FinalizedMessage {
                    message: LegacyMessage {
                        id: "rec-1".into(),
                        r#type: MessageType::Message,
                        role: LegacyRole::Assistant,
                        content: Vec::new(),
                        model: "rec".into(),
                        stop_reason: Some(LegacyStopReason::EndTurn),
                        stop_sequence: None,
                        stop_details: None,
                        usage: LegacyUsage::default(),
                        container: None,
                        context_management: None,
                    },
                }];
                Ok(Box::pin(async_stream::stream! {
                    for e in events {
                        yield e;
                    }
                }))
            }

            fn model_name(&self) -> String {
                "recording".into()
            }

            fn model_url(&self) -> String {
                "recording://".into()
            }
        }

        let seen = StdArc::new(StdMutex::new(None));
        let model: Arc<dyn Model> = Arc::new(RecordingModel {
            seen: StdArc::clone(&seen),
        });
        let adapter = LegacyProviderAdapter::new(model);
        let info = fake_model_info();
        let options = SimpleStreamOptions {
            base: StreamOptions::default(),
            reasoning: Some(ThinkingLevel::High),
        };
        let stream = adapter.stream_simple(&info, &Context::new(""), &options);
        let _ = drain(stream).await;
        let recorded = seen.lock().unwrap().clone();
        assert!(matches!(recorded, Some(ThinkingConfig::High)));
    }

    /// Round-trip: project a unified `Message::ToolResult` back to a
    /// legacy `MessageParam` and verify the structure (`Role::User`
    /// carrying a single `ToolResultBlock`).
    #[test]
    fn tool_result_message_projects_to_user_role_tool_result_block() {
        let trm = ToolResultMessage {
            tool_call_id: "tu-1".into(),
            tool_name: "ping".into(),
            content: vec![UserContent::text("pong")],
            details: None,
            is_error: false,
            timestamp: 0,
        };
        let projected = project_tool_result_message(&trm);
        assert!(matches!(projected.role, Role::User));
        assert_eq!(projected.content.len(), 1);
        match &projected.content[0] {
            ContentBlockParam::ToolResultBlock {
                tool_use_id,
                content,
                is_error,
            } => {
                assert_eq!(tool_use_id, "tu-1");
                assert!(!*is_error);
                match content {
                    ToolResultContent::Blocks(blocks) => {
                        assert_eq!(blocks.len(), 1);
                        match &blocks[0] {
                            ContentBlockParam::TextBlock { text, .. } => {
                                assert_eq!(text, "pong");
                            }
                            other => panic!("expected text block, got {other:?}"),
                        }
                    }
                    other => panic!("expected blocks variant, got {other:?}"),
                }
            }
            other => panic!("expected tool result block, got {other:?}"),
        }
    }

    /// Round-trip a thinking block from unified to legacy and back, to
    /// make sure signature handling stays symmetric.
    #[test]
    fn thinking_round_trip_preserves_signature() {
        let unified = AssistantContent::Thinking(ThinkingContent {
            thinking: "I'm thinking".into(),
            thinking_signature: Some("sig-abc".into()),
            redacted: false,
        });
        let legacy = project_assistant_content(&unified);
        match &legacy {
            ContentBlockParam::ThinkingBlock {
                signature,
                thinking,
            } => {
                assert_eq!(signature, "sig-abc");
                assert_eq!(thinking, "I'm thinking");
            }
            other => panic!("expected thinking block, got {other:?}"),
        }
        // And the legacy → unified direction (used during FinalizedMessage
        // reconstruction).
        let legacy_block = LegacyContentBlock::ThinkingBlock {
            signature: "sig-abc".into(),
            thinking: "I'm thinking".into(),
        };
        let round = map_legacy_block_to_assistant(&legacy_block).expect("mapped");
        match round {
            AssistantContent::Thinking(t) => {
                assert_eq!(t.thinking_signature.as_deref(), Some("sig-abc"));
                assert_eq!(t.thinking, "I'm thinking");
                assert!(!t.redacted);
            }
            _ => panic!("expected thinking content"),
        }
    }

    /// Caller field on a tool_use round-trips as `None` since unified
    /// `AssistantContent::ToolCall` doesn't model caller scoping.
    #[test]
    fn tool_call_projection_drops_caller_field() {
        let unified = AssistantContent::ToolCall(ToolCall {
            id: "tu".into(),
            name: "ping".into(),
            arguments: serde_json::json!({}),
        });
        let projected = project_assistant_content(&unified);
        match projected {
            ContentBlockParam::ToolUseBlock { caller, .. } => assert!(caller.is_none()),
            _ => panic!("expected tool use block"),
        }

        // Conversely, a legacy block with a caller still projects to an
        // `AssistantContent::ToolCall` (caller field is dropped, but the
        // call survives so the model sees its prior request).
        let legacy_block = LegacyContentBlock::ToolUseBlock {
            id: "tu".into(),
            name: "ping".into(),
            input: Value::Null,
            caller: Some(Caller::Direct),
        };
        let mapped = map_legacy_block_to_assistant(&legacy_block).expect("mapped");
        assert!(matches!(mapped, AssistantContent::ToolCall(_)));
    }
}
