//! OpenAI Chat Completions API provider.
//!
//! Implements the unified [`Provider`] trait against OpenAI's
//! `POST /chat/completions` streaming endpoint. See
//! `docs/models-spec.md` §7.2.
//!
//! Stateless — per-call HTTP knobs (auth, base URL, reasoning effort,
//! tool choice) are derived from the per-call [`ModelInfo`] and
//! [`StreamOptions`] so the same instance can serve any number of
//! concurrent requests.
//!
//! Reasoning is **one-way** on this provider: streaming
//! `delta.reasoning_content` is captured into a [`ThinkingContent`]
//! block so the UI can render it live, but prior-turn thinking blocks
//! are dropped on outbound requests because the public Chat
//! Completions API does not accept reasoning content on input.
//! Multi-turn reasoning continuity requires the OpenAI Responses
//! provider instead.

use std::collections::HashMap;

use futures::StreamExt;
use openai_sdk::client::{Client, ClientError};
use openai_sdk::types::chat_completions::{
    ChatCompletionRequestMessage, ChatCompletionTextContent, ChatCompletionUserContent,
    ChatCompletionUserContentPart, CreateChatCompletionRequest, CreateChatCompletionStreamResponse,
    FinishReason, FunctionChoice, FunctionDefinition, ImageUrl, StreamOptions as ChatStreamOptions,
    Tool as ChatTool, ToolCall as ChatToolCall, ToolChoice as ChatToolChoice, Usage as ChatUsage,
};
use openai_sdk::types::common::ReasoningEffort;
use serde_json::Value;

use crate::errors::{
    classify_openai_error, classify_openai_finish_reason, parse_retry_after, transport_error,
};
use crate::partial_json::parse_streaming_json;
use crate::provider::Provider;
use crate::registry::{ModelInfo, calculate_cost, supports_xhigh};
use crate::streaming::{
    AssistantMessageEvent, AssistantMessageEventStream, DoneReason, ErrorReason,
};
use crate::transform::transform_messages;
use crate::types::{
    AssistantContent, AssistantError, AssistantMessage, Context, ErrorCategory, ImageContent,
    Message, SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingContent,
    ThinkingLevel, ToolCall, ToolChoice, ToolDefinition, ToolResultMessage, Usage, UserContent,
    UserMessage,
};

/// `api` field reported on assistant messages produced by this provider.
const API_NAME: &str = "openai-completions";

/// Stateless provider for the OpenAI Chat Completions API.
pub struct OpenAiCompletionsProvider;

impl Provider for OpenAiCompletionsProvider {
    fn stream(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream {
        spawn_stream(model.clone(), context.clone(), options.clone(), None)
    }

    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream {
        spawn_stream(
            model.clone(),
            context.clone(),
            options.base.clone(),
            options.reasoning.clone(),
        )
    }
}

// ---------------------------------------------------------------------------
// Stream entry point
// ---------------------------------------------------------------------------

/// Build the stream handle synchronously and spawn a tokio task that drives
/// the underlying SSE response. The task owns its own clone of the stream
/// handle and pushes events as they arrive.
fn spawn_stream(
    model: ModelInfo,
    context: Context,
    options: StreamOptions,
    reasoning: Option<ThinkingLevel>,
) -> AssistantMessageEventStream {
    let stream = AssistantMessageEventStream::new();
    let producer = stream.clone();
    tokio::spawn(async move {
        run_stream(producer.clone(), model, context, options, reasoning).await;
        // Safety net: if `run_stream` exited without emitting a terminal
        // event the stream would otherwise hang. `end()` is a no-op once
        // a terminal event has been pushed.
        producer.end();
    });
    stream
}

/// Drive a single inference call. On any pre-stream failure (auth, request
/// shape, network setup) emits a synthetic [`AssistantMessageEvent::Error`]
/// onto the stream so callers always observe a terminal event.
async fn run_stream(
    producer: AssistantMessageEventStream,
    model: ModelInfo,
    context: Context,
    options: StreamOptions,
    reasoning: Option<ThinkingLevel>,
) {
    if let Err(err) =
        run_stream_inner(&producer, &model, &context, &options, reasoning.as_ref()).await
    {
        let mut error = AssistantMessage::empty();
        error.api = API_NAME.to_string();
        error.provider = model.provider.clone();
        error.model = model.id.clone();
        error.stop_reason = StopReason::Error;
        error.error = Some(err);
        producer.push(AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error,
        });
    }
}

/// Inner entrypoint that returns `Err(AssistantError)` on pre-stream
/// failures (so the outer task can surface them as a uniform `Error`
/// event) and `Ok(())` once the SSE stream has been fully consumed
/// and a terminal event has been pushed.
async fn run_stream_inner(
    producer: &AssistantMessageEventStream,
    model: &ModelInfo,
    context: &Context,
    options: &StreamOptions,
    reasoning: Option<&ThinkingLevel>,
) -> Result<(), AssistantError> {
    let api_key = options.resolve_api_key().await.map_err(|err| {
        AssistantError::new(
            ErrorCategory::Auth,
            format!("openai-completions provider: {err}"),
        )
    })?;

    let base_url = (!model.base_url.is_empty()).then(|| model.base_url.clone());
    let client = Client::new(base_url, api_key);
    let request = build_request(model, context, options, reasoning);

    if let Some(cb) = options.on_payload.as_ref() {
        match serde_json::to_value(&request) {
            Ok(body) => cb.call(&body),
            Err(err) => tracing::warn!("on_payload serialization failed: {err}"),
        }
    }

    let mut sse = client
        .chat_completions_stream(request)
        .await
        .map_err(|err| classify_client_error(&err))?;

    let mut state = StreamState::new(model);
    let mut saw_terminal = false;

    while let Some(event) = sse.next().await {
        match event {
            Ok(chunk) => {
                let outcome = state.process(chunk);
                for ev in outcome.events {
                    producer.push(ev);
                }
                if outcome.terminal {
                    saw_terminal = true;
                    break;
                }
            }
            Err(err) => {
                return Err(classify_client_error(&err));
            }
        }
    }

    if !saw_terminal {
        tracing::debug!("openai-completions stream closed without finish_reason; finalizing");
    }
    let final_event = state.finalize();
    producer.push(final_event);
    Ok(())
}

/// Classify a transport-layer or SDK-surfaced error into the unified
/// [`AssistantError`] shape per `docs/models-spec.md` §10.3.
fn classify_client_error(err: &ClientError) -> AssistantError {
    match err {
        ClientError::ApiError {
            error,
            http_status,
            retry_after,
        } => classify_openai_error(
            error.code.as_deref(),
            error.r#type.as_deref(),
            Some(*http_status),
            parse_retry_after(retry_after.as_deref()),
            error.message.clone(),
        ),
        ClientError::TransportError(t) => transport_error(format!("transport: {t}")),
        ClientError::ParseError(s) => transport_error(format!("parse: {s}")),
        ClientError::InternalError(s) => transport_error(format!("internal: {s}")),
    }
}

// ---------------------------------------------------------------------------
// Request body construction (§7.2)
// ---------------------------------------------------------------------------

fn build_request(
    model: &ModelInfo,
    context: &Context,
    options: &StreamOptions,
    reasoning: Option<&ThinkingLevel>,
) -> CreateChatCompletionRequest {
    let mut messages = Vec::new();
    if let Some(prompt) = context.system_prompt.as_deref()
        && !prompt.is_empty()
    {
        messages.push(build_system_message(model, prompt));
    }
    // §8: rewrite the history for cross-provider replay before
    // serializing into Chat Completions request messages.
    let transformed = transform_messages(&context.messages, model);
    convert_messages(&transformed, &mut messages);

    let tools: Vec<ChatTool> = context.tools.iter().map(to_chat_tool).collect();
    let tool_choice = to_chat_tool_choice(options.tool_choice.as_ref(), !tools.is_empty());

    // §7.2: temperature is set normally even when reasoning is on
    // (unlike Anthropic, where extended thinking conflicts with it).
    let temperature = options.temperature;

    // Map our `max_tokens` onto the API's `max_completion_tokens`.
    // OpenAI's `max_tokens` is deprecated, and reasoning models reject
    // it in favor of the new field. Clamp to `u32::MAX` defensively;
    // catalog values stay well below that ceiling.
    let max_completion_tokens = options
        .max_tokens
        .map(|t| u32::try_from(t).unwrap_or(u32::MAX));

    let reasoning_effort = if model.reasoning {
        Some(map_reasoning_effort(reasoning, model))
    } else {
        // Non-reasoning models reject the field entirely.
        None
    };

    #[allow(deprecated)]
    let mut request = CreateChatCompletionRequest {
        model: model.id.clone(),
        messages,
        max_completion_tokens,
        max_tokens: None,
        temperature,
        top_p: None,
        n: None,
        presence_penalty: None,
        frequency_penalty: None,
        logit_bias: None,
        response_format: None,
        stop: None,
        stream: Some(true),
        // §7.2: request usage in streaming so we can populate `Usage`.
        stream_options: Some(ChatStreamOptions {
            include_usage: Some(true),
            include_obfuscation: None,
        }),
        tools,
        tool_choice,
        parallel_tool_calls: None,
        functions: Vec::new(),
        function_call: None,
        logprobs: None,
        top_logprobs: None,
        modalities: None,
        audio: None,
        reasoning_effort,
        verbosity: None,
        prediction: None,
        seed: None,
        // §7.2: explicitly send `store: false` so conversations are
        // never persisted server-side, even if the API default changes.
        store: Some(false),
        web_search_options: None,
        metadata: None,
        user: None,
        safety_identifier: None,
        prompt_cache_key: None,
        prompt_cache_retention: None,
        service_tier: None,
    };

    // String metadata is the only kind Chat Completions accepts; drop
    // anything non-string in the per-call `metadata` map.
    if let Some(extra) = options.metadata.as_ref() {
        let filtered: HashMap<String, String> = extra
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
        if !filtered.is_empty() {
            request.metadata = Some(filtered);
        }
    }

    request
}

/// Build the system / developer message for the request.
///
/// §7.2: reasoning-capable models receive the prompt as a `developer`
/// turn (consistent with OpenAI's GPT-5 / o-series convention);
/// everything else takes the classic `system` slot.
fn build_system_message(model: &ModelInfo, prompt: &str) -> ChatCompletionRequestMessage {
    if model.reasoning {
        ChatCompletionRequestMessage::developer(prompt.to_string())
    } else {
        ChatCompletionRequestMessage::system(prompt.to_string())
    }
}

// ---------------------------------------------------------------------------
// Message conversion (§7.2 "Message conversion")
// ---------------------------------------------------------------------------

/// Project the unified message log onto Chat Completions request messages.
///
/// Tool results emit a `tool` message followed by a synthetic `user`
/// message carrying any image parts (the API doesn't accept images
/// inside `tool` content). Empty assistant messages — typically the
/// residue of an aborted prior stream — are dropped because the API
/// rejects assistant turns that are entirely empty.
fn convert_messages(messages: &[Message], out: &mut Vec<ChatCompletionRequestMessage>) {
    for msg in messages {
        match msg {
            Message::User(u) => {
                out.push(convert_user_message(u));
            }
            Message::Assistant(a) => {
                if let Some(m) = convert_assistant_message(a) {
                    out.push(m);
                }
            }
            Message::ToolResult(tr) => {
                let (tool_msg, image_followup) = convert_tool_result(tr);
                out.push(tool_msg);
                if let Some(followup) = image_followup {
                    out.push(followup);
                }
            }
        }
    }
}

fn convert_user_message(m: &UserMessage) -> ChatCompletionRequestMessage {
    let parts: Vec<ChatCompletionUserContentPart> =
        m.content.iter().map(user_content_to_part).collect();
    let content = if parts.iter().all(is_text_part) {
        // All-text turns serialize as the cheaper `string` shape so the
        // wire matches what most callers expect.
        let combined = parts
            .iter()
            .filter_map(|p| match p {
                ChatCompletionUserContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        ChatCompletionUserContent::String(combined)
    } else {
        ChatCompletionUserContent::Array(parts)
    };
    ChatCompletionRequestMessage::User {
        content,
        name: None,
    }
}

fn user_content_to_part(c: &UserContent) -> ChatCompletionUserContentPart {
    match c {
        UserContent::Text(t) => ChatCompletionUserContentPart::Text {
            text: t.text.clone(),
        },
        UserContent::Image(img) => ChatCompletionUserContentPart::ImageUrl {
            image_url: ImageUrl {
                url: format!("data:{};base64,{}", img.mime_type, img.data),
                detail: None,
            },
        },
    }
}

fn is_text_part(p: &ChatCompletionUserContentPart) -> bool {
    matches!(p, ChatCompletionUserContentPart::Text { .. })
}

/// Translate an [`AssistantMessage`] into a Chat Completions
/// `assistant` turn.
///
/// §7.2: thinking blocks are dropped (reasoning is not accepted on
/// input here), tool calls flatten into the `tool_calls` array, and
/// text blocks concatenate into a single string. Returns `None` when
/// the assistant message carries no text and no tool calls — empty
/// assistant turns get rejected by the API.
fn convert_assistant_message(m: &AssistantMessage) -> Option<ChatCompletionRequestMessage> {
    let mut text_buf = String::new();
    let mut tool_calls: Vec<ChatToolCall> = Vec::new();

    for block in &m.content {
        match block {
            AssistantContent::Text(t) => text_buf.push_str(&t.text),
            AssistantContent::Thinking(_) => {
                // Dropped on outbound per §7.2.
            }
            AssistantContent::ToolCall(tc) => {
                tool_calls.push(ChatToolCall::Function {
                    id: tc.id.clone(),
                    function: openai_sdk::types::chat_completions::FunctionCall {
                        name: tc.name.clone(),
                        arguments: tc.arguments.to_string(),
                    },
                });
            }
        }
    }

    if text_buf.is_empty() && tool_calls.is_empty() {
        return None;
    }

    let content = if text_buf.is_empty() {
        None
    } else {
        Some(text_buf)
    };
    Some(ChatCompletionRequestMessage::Assistant {
        content,
        refusal: None,
        tool_calls,
        audio: None,
        name: None,
    })
}

/// Translate a [`ToolResultMessage`] into a `tool` message plus an
/// optional `user` follow-up carrying any image parts.
///
/// §7.2: Chat Completions' `tool` role only accepts text, so image
/// attachments from the tool ride along as a subsequent `user` turn.
fn convert_tool_result(
    t: &ToolResultMessage,
) -> (
    ChatCompletionRequestMessage,
    Option<ChatCompletionRequestMessage>,
) {
    let mut text_buf = String::new();
    let mut image_parts: Vec<ChatCompletionUserContentPart> = Vec::new();

    for c in &t.content {
        match c {
            UserContent::Text(text) => text_buf.push_str(&text.text),
            UserContent::Image(img) => {
                image_parts.push(user_content_to_part(&UserContent::Image(ImageContent {
                    data: img.data.clone(),
                    mime_type: img.mime_type.clone(),
                })))
            }
        }
    }

    // Tool messages are required to have non-empty content. Surface the
    // error case so the model still has *something* to react to.
    if text_buf.is_empty() {
        text_buf = if t.is_error {
            "[tool returned an error with no text payload]".to_string()
        } else {
            "[tool returned no text]".to_string()
        };
    }

    let tool_msg = ChatCompletionRequestMessage::Tool {
        content: ChatCompletionTextContent::String(text_buf),
        tool_call_id: t.tool_call_id.clone(),
    };

    let image_followup = if image_parts.is_empty() {
        None
    } else {
        Some(ChatCompletionRequestMessage::User {
            content: ChatCompletionUserContent::Array(image_parts),
            name: None,
        })
    };

    (tool_msg, image_followup)
}

// ---------------------------------------------------------------------------
// Public round-trip helpers (`docs/models-spec.md` §1.10, §12 step 11b)
// ---------------------------------------------------------------------------

/// Project an [`AssistantMessage`] onto the Chat Completions request item
/// shape — the [`ChatCompletionRequestMessage::Assistant`] entry that
/// gets sent as part of `messages[]` on a follow-up turn.
///
/// Serialize side of the §1.10 round-trip invariant for
/// `openai-completions`. Same projection the provider uses internally
/// when building a request body, exposed publicly so the round-trip
/// test suite (and any future caller that wants a single assistant turn
/// materialized into its wire form) can reach it directly.
///
/// Behaviour follows §7.2:
///
/// - Text blocks concatenate into the single `content` string the API
///   accepts.
/// - Thinking blocks are dropped — the public Chat Completions endpoint
///   does not accept `reasoning_content` on input. §1.10 explicitly
///   lists reasoning under the deliberately-not-preserved set for this
///   provider; see §7.2 for the rationale.
/// - Tool calls flatten into the `tool_calls` array, with each block's
///   JSON [`Value`] arguments serialized to a string per the wire shape.
///
/// Returns `None` when the assistant turn carries no text and no tool
/// calls — empty assistant messages are rejected by the API. Callers
/// projecting a streamed message that has at least one block can rely
/// on `Some(...)`; the streaming parser never produces empty messages
/// once any delta has arrived.
pub fn assistant_message_to_request_item(
    message: &AssistantMessage,
) -> Option<ChatCompletionRequestMessage> {
    convert_assistant_message(message)
}

/// Inverse of [`assistant_message_to_request_item`]: parse a Chat
/// Completions `messages[]` entry whose role is `assistant` back into a
/// unified [`AssistantMessage`].
///
/// Parse side of the §1.10 round-trip invariant for
/// `openai-completions` — symmetric to the streaming state machine in
/// [`StreamState`], because Chat Completions' request and response
/// assistant content share shapes one-for-one. The mapping from
/// `docs/models-spec.md` §7.2 preserved here:
///
/// - `content` (`Option<String>`) → [`AssistantContent::Text`] when
///   non-empty.
/// - `refusal` → another [`AssistantContent::Text`] block, matching the
///   streaming parser's treatment of `delta.refusal` (the unified shape
///   does not carry a separate refusal channel).
/// - `tool_calls` → [`AssistantContent::ToolCall`], one per entry, in
///   array order. The wire `function.arguments` string is parsed as
///   strict JSON first, with [`parse_streaming_json`] as a fallback so
///   a malformed prior turn still yields a structured `Value` rather
///   than failing the parse.
///
/// Reasoning is intentionally not represented on the request side and
/// therefore not recovered here — see §7.2 / §1.10.
///
/// Non-`Assistant` variants (`User`, `System`, `Tool`, `Developer`)
/// produce an empty `AssistantMessage`; the role is taken on faith,
/// matching [`crate::anthropic::provider::parse_assistant_request_item`].
pub fn parse_assistant_request_item(item: &ChatCompletionRequestMessage) -> AssistantMessage {
    let mut out = AssistantMessage::empty();
    out.api = API_NAME.to_string();
    if let ChatCompletionRequestMessage::Assistant {
        content,
        refusal,
        tool_calls,
        ..
    } = item
    {
        if let Some(text) = content.as_deref()
            && !text.is_empty()
        {
            out.content.push(AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            }));
        }
        if let Some(text) = refusal.as_deref()
            && !text.is_empty()
        {
            // Refusals on the wire surface as text in the unified shape;
            // this matches the streaming parser's handling of
            // `delta.refusal`, which routes through `handle_text_delta`.
            out.content.push(AssistantContent::Text(TextContent {
                text: text.to_string(),
                text_signature: None,
            }));
        }
        for tc in tool_calls {
            match tc {
                ChatToolCall::Function { id, function } => {
                    let arguments: Value = if function.arguments.is_empty() {
                        Value::Object(serde_json::Map::new())
                    } else {
                        serde_json::from_str(&function.arguments)
                            .unwrap_or_else(|_| parse_streaming_json(&function.arguments))
                    };
                    out.content.push(AssistantContent::ToolCall(ToolCall {
                        id: id.clone(),
                        name: function.name.clone(),
                        arguments,
                    }));
                }
                // Custom tools are out of the agent's tool surface; drop
                // them, matching the request-build side which only ever
                // emits function tools.
                ChatToolCall::Custom { .. } => {}
            }
        }
    }
    out
}

/// Replay a sequence of pre-decoded Chat Completions stream chunks
/// through the provider's streaming state machine and return the
/// finalized [`AssistantMessage`].
///
/// Mirror of [`crate::anthropic::provider::replay_sse_events`] in
/// shape; the fixture-based round-trip suite uses this to turn captured
/// SSE wire dumps into unified messages without spinning up a real HTTP
/// client. Provided publicly so external test suites can share the
/// exact same parse path the live provider does.
pub fn replay_sse_events(
    model: &ModelInfo,
    events: impl IntoIterator<Item = CreateChatCompletionStreamResponse>,
) -> AssistantMessage {
    let mut state = StreamState::new(model);
    for chunk in events {
        // We deliberately discard the per-chunk events: the round-trip
        // suite only cares about the finalized terminal message, and
        // `state.finalize()` rebuilds it from the running snapshot.
        let _ = state.process(chunk);
    }
    match state.finalize() {
        AssistantMessageEvent::Done { message, .. }
        | AssistantMessageEvent::Error { error: message, .. } => message,
        other => panic!("StreamState::finalize returned non-terminal event: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tools / tool choice (§7.2 "Tool definition format" + "Tool choice mapping")
// ---------------------------------------------------------------------------

fn to_chat_tool(tool: &ToolDefinition) -> ChatTool {
    ChatTool::Function {
        function: FunctionDefinition {
            name: tool.name.clone(),
            description: Some(tool.description.clone()),
            parameters: tool.parameters.clone(),
            // §7.2 hardcodes `strict: false` — strict mode rejects
            // many of our tool schemas (open-ended object shapes).
            strict: Some(false),
        },
    }
}

fn to_chat_tool_choice(choice: Option<&ToolChoice>, has_tools: bool) -> Option<ChatToolChoice> {
    match choice {
        None => None,
        // The API rejects every flavor of tool_choice when no tools
        // were declared — match Anthropic's behavior and omit the
        // field rather than send something the server will refuse.
        _ if !has_tools => None,
        Some(ToolChoice::Auto) => Some(ChatToolChoice::String("auto".to_string())),
        Some(ToolChoice::None) => Some(ChatToolChoice::String("none".to_string())),
        Some(ToolChoice::Required) => Some(ChatToolChoice::String("required".to_string())),
        Some(ToolChoice::Tool { name }) => Some(ChatToolChoice::Object {
            r#type: "function".to_string(),
            function: FunctionChoice { name: name.clone() },
        }),
    }
}

// ---------------------------------------------------------------------------
// Reasoning effort (§7.2 "Reasoning effort")
// ---------------------------------------------------------------------------

fn map_reasoning_effort(level: Option<&ThinkingLevel>, model: &ModelInfo) -> ReasoningEffort {
    match level {
        None => ReasoningEffort::None,
        Some(ThinkingLevel::Minimal) => ReasoningEffort::Minimal,
        Some(ThinkingLevel::Low) => ReasoningEffort::Low,
        Some(ThinkingLevel::Medium) => ReasoningEffort::Medium,
        Some(ThinkingLevel::High) => ReasoningEffort::High,
        Some(ThinkingLevel::XHigh) => {
            if supports_xhigh(model) {
                ReasoningEffort::XHigh
            } else {
                // §7.2: XHigh is GPT-5.2+ only; everything else falls
                // back to High so the request still validates.
                ReasoningEffort::High
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Stream state machine (§7.2 "Stream event mapping")
// ---------------------------------------------------------------------------

/// Per-content-block routing state. We thread incoming deltas to the
/// matching slot in the running `partial.content` vector by wire role:
/// the API sends content (text), reasoning_content (thinking), and
/// tool_calls separately, but our unified model sequences them as a
/// single `Vec<AssistantContent>`.
struct StreamState {
    /// Running snapshot of the assistant message; cloned into every
    /// emitted event.
    partial: AssistantMessage,
    /// Whether `Start` has been emitted yet.
    started: bool,
    /// Index of the in-progress `Text` block within `partial.content`,
    /// if any. Reset to `None` on `TextEnd` (next text delta opens a
    /// new block — though in practice Chat Completions only emits one
    /// text block per response).
    text_index: Option<usize>,
    /// Index of the in-progress `Thinking` block within
    /// `partial.content`, if any.
    thinking_index: Option<usize>,
    /// Map from the wire's `tool_calls[i].index` to the unified
    /// `partial.content[content_index]` slot. Each entry tracks the
    /// cumulative arguments JSON bytes alongside the index so we can
    /// re-parse on every delta.
    tool_calls: HashMap<i32, ToolCallSlot>,
    /// Latest finish_reason seen across choices. Set on the chunk
    /// that carried a non-null finish_reason; finalized into a
    /// terminal event on stream end.
    finish_reason: Option<FinishReason>,
    /// Latest streamed usage — only the last chunk carries the real
    /// totals; replace each time we see one.
    usage: Option<ChatUsage>,
}

struct ToolCallSlot {
    /// Index into `partial.content`.
    content_index: usize,
    /// Cumulative arguments bytes received so far.
    arguments: String,
}

/// Result of processing a single stream chunk.
struct ProcessOutcome {
    events: Vec<AssistantMessageEvent>,
    /// Whether the upstream stream has terminated. The API closes the
    /// stream after the chunk that carries `finish_reason`, but we
    /// don't `break` on it — the next chunk usually carries usage —
    /// so this stays `false` here. Reserved for protocol-level
    /// terminators if any get added.
    terminal: bool,
}

impl StreamState {
    fn new(model: &ModelInfo) -> Self {
        let mut partial = AssistantMessage::empty();
        partial.api = API_NAME.to_string();
        partial.provider = model.provider.clone();
        partial.model = model.id.clone();
        Self {
            partial,
            started: false,
            text_index: None,
            thinking_index: None,
            tool_calls: HashMap::new(),
            finish_reason: None,
            usage: None,
        }
    }

    fn process(&mut self, chunk: CreateChatCompletionStreamResponse) -> ProcessOutcome {
        let mut events = Vec::new();

        if !self.started {
            self.started = true;
            self.partial.response_id = Some(chunk.id.clone());
            if !chunk.model.is_empty() {
                self.partial.model = chunk.model.clone();
            }
            events.push(AssistantMessageEvent::Start {
                partial: self.partial.clone(),
            });
        } else if !chunk.model.is_empty() && self.partial.model.is_empty() {
            self.partial.model = chunk.model.clone();
        }

        if let Some(usage) = chunk.usage.as_ref() {
            self.usage = Some(usage.clone());
        }

        for choice in chunk.choices {
            // Text delta.
            if let Some(text) = choice.delta.content.as_deref()
                && !text.is_empty()
            {
                self.handle_text_delta(text, &mut events);
            }

            // Reasoning delta (alias for `reasoning`/`reasoning_text`
            // captured by the SDK's serde aliases).
            if let Some(thinking) = choice.delta.reasoning_content.as_deref()
                && !thinking.is_empty()
            {
                self.handle_thinking_delta(thinking, &mut events);
            }

            // Refusals come through as text in the unified shape — the
            // model says "I can't do that" the same way regardless of
            // wire-level packaging.
            if let Some(refusal) = choice.delta.refusal.as_deref()
                && !refusal.is_empty()
            {
                self.handle_text_delta(refusal, &mut events);
            }

            // Tool call deltas.
            for tc_delta in &choice.delta.tool_calls {
                self.handle_tool_call_delta(tc_delta, &mut events);
            }

            // Finish reason on this choice closes any in-flight blocks
            // and is captured for the terminal event.
            if let Some(finish) = choice.finish_reason.clone() {
                self.close_open_blocks(&mut events);
                self.finish_reason = Some(finish);
            }
        }

        ProcessOutcome {
            events,
            terminal: false,
        }
    }

    fn handle_text_delta(&mut self, text: &str, events: &mut Vec<AssistantMessageEvent>) {
        // If a thinking block was open, close it — text following
        // reasoning starts a new block.
        if self.thinking_index.is_some() {
            self.close_thinking(events);
        }

        let content_index = match self.text_index {
            Some(idx) => idx,
            None => {
                let idx = self.partial.content.len();
                self.partial
                    .content
                    .push(AssistantContent::Text(TextContent {
                        text: String::new(),
                        text_signature: None,
                    }));
                self.text_index = Some(idx);
                events.push(AssistantMessageEvent::TextStart {
                    content_index: idx,
                    partial: self.partial.clone(),
                });
                idx
            }
        };

        if let Some(AssistantContent::Text(t)) = self.partial.content.get_mut(content_index) {
            t.text.push_str(text);
        }
        events.push(AssistantMessageEvent::TextDelta {
            content_index,
            delta: text.to_string(),
            partial: self.partial.clone(),
        });
    }

    fn handle_thinking_delta(&mut self, thinking: &str, events: &mut Vec<AssistantMessageEvent>) {
        // Reasoning interleaves before text; if a text block is open
        // we close it first so the order on the wire is preserved.
        if self.text_index.is_some() {
            self.close_text(events);
        }

        let content_index = match self.thinking_index {
            Some(idx) => idx,
            None => {
                let idx = self.partial.content.len();
                self.partial
                    .content
                    .push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    }));
                self.thinking_index = Some(idx);
                events.push(AssistantMessageEvent::ThinkingStart {
                    content_index: idx,
                    partial: self.partial.clone(),
                });
                idx
            }
        };

        if let Some(AssistantContent::Thinking(t)) = self.partial.content.get_mut(content_index) {
            t.thinking.push_str(thinking);
        }
        events.push(AssistantMessageEvent::ThinkingDelta {
            content_index,
            delta: thinking.to_string(),
            partial: self.partial.clone(),
        });
    }

    fn handle_tool_call_delta(
        &mut self,
        delta: &openai_sdk::types::chat_completions::ToolCallDelta,
        events: &mut Vec<AssistantMessageEvent>,
    ) {
        // Ensure prose blocks are closed before we open a tool call
        // so the unified content order matches user expectation
        // (text → reasoning → tool calls).
        if self.text_index.is_some() {
            self.close_text(events);
        }
        if self.thinking_index.is_some() {
            self.close_thinking(events);
        }

        let wire_index = delta.index;
        let slot = self.tool_calls.entry(wire_index).or_insert_with(|| {
            let content_index = self.partial.content.len();
            self.partial
                .content
                .push(AssistantContent::ToolCall(ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: Value::Object(serde_json::Map::new()),
                }));
            ToolCallSlot {
                content_index,
                arguments: String::new(),
            }
        });

        let content_index = slot.content_index;
        let mut emit_start = false;
        let mut delta_arg_str: Option<String> = None;

        if let Some(AssistantContent::ToolCall(tc)) = self.partial.content.get_mut(content_index) {
            // First touch of this slot fixes id and name; either field
            // appearing for the first time signals the block start.
            if let Some(id) = delta.id.as_deref()
                && !id.is_empty()
                && tc.id.is_empty()
            {
                tc.id = id.to_string();
                emit_start = true;
            }
            if let Some(func) = delta.function.as_ref() {
                if let Some(name) = func.name.as_deref()
                    && !name.is_empty()
                    && tc.name.is_empty()
                {
                    tc.name = name.to_string();
                    emit_start = true;
                }
                if let Some(args) = func.arguments.as_deref()
                    && !args.is_empty()
                {
                    slot.arguments.push_str(args);
                    tc.arguments = parse_streaming_json(&slot.arguments);
                    delta_arg_str = Some(args.to_string());
                }
            }
        }

        // Order on the wire: Start before Delta. Both pushes need a
        // clone of `self.partial` so we do them once the mutable borrow
        // above is released.
        if emit_start {
            events.push(AssistantMessageEvent::ToolCallStart {
                content_index,
                partial: self.partial.clone(),
            });
        }
        if let Some(arg_str) = delta_arg_str {
            events.push(AssistantMessageEvent::ToolCallDelta {
                content_index,
                delta: arg_str,
                partial: self.partial.clone(),
            });
        }
    }

    fn close_text(&mut self, events: &mut Vec<AssistantMessageEvent>) {
        if let Some(idx) = self.text_index.take() {
            let content = match self.partial.content.get(idx) {
                Some(AssistantContent::Text(t)) => t.text.clone(),
                _ => String::new(),
            };
            events.push(AssistantMessageEvent::TextEnd {
                content_index: idx,
                content,
                partial: self.partial.clone(),
            });
        }
    }

    fn close_thinking(&mut self, events: &mut Vec<AssistantMessageEvent>) {
        if let Some(idx) = self.thinking_index.take() {
            let content = match self.partial.content.get(idx) {
                Some(AssistantContent::Thinking(t)) => t.thinking.clone(),
                _ => String::new(),
            };
            events.push(AssistantMessageEvent::ThinkingEnd {
                content_index: idx,
                content,
                partial: self.partial.clone(),
            });
        }
    }

    /// Close any in-flight content blocks. Called when the wire
    /// signals end-of-message via `finish_reason`. Tool-call End
    /// events pull their canonical `arguments` value from a strict
    /// reparse (falling back to the streaming partial parser if the
    /// model produced malformed JSON, in which case we synthesize an
    /// empty object).
    fn close_open_blocks(&mut self, events: &mut Vec<AssistantMessageEvent>) {
        self.close_text(events);
        self.close_thinking(events);

        // Emit ToolCallEnd events in source order. The wire sends
        // tool calls with stable indices, so we sort by `wire_index`
        // to keep ordering deterministic.
        let mut entries: Vec<(i32, ToolCallSlot)> = self.tool_calls.drain().collect();
        entries.sort_by_key(|(idx, _)| *idx);
        for (_, slot) in entries {
            let content_index = slot.content_index;
            // Final, definitive parse: the partial parser already tries
            // strict JSON first and escalates through repair / completion
            // before falling back to an empty object.
            let parsed: Value = parse_streaming_json(&slot.arguments);
            let mut tool_call_snapshot = None;
            if let Some(AssistantContent::ToolCall(tc)) =
                self.partial.content.get_mut(content_index)
            {
                tc.arguments = parsed.clone();
                tool_call_snapshot = Some(tc.clone());
            }
            if let Some(tool_call) = tool_call_snapshot {
                events.push(AssistantMessageEvent::ToolCallEnd {
                    content_index,
                    tool_call,
                    partial: self.partial.clone(),
                });
            }
        }
    }

    /// Build the terminal event that wraps up the stream.
    fn finalize(mut self) -> AssistantMessageEvent {
        // Defensive: if the upstream stream closed without surfacing
        // a finish_reason for every choice, close any blocks we
        // still have open here.
        let mut tail = Vec::new();
        self.close_open_blocks(&mut tail);
        // Drop the intermediate close-block events on the floor —
        // they should already have been emitted on the chunk that
        // carried `finish_reason`. We keep them only if the stream
        // ended abruptly, in which case attaching them to the
        // terminal event is too late anyway; we emit a synthetic
        // `Done` directly instead.
        let _ = tail;

        if let Some(usage) = self.usage.as_ref() {
            apply_usage(&mut self.partial.usage, usage);
        }
        let cost_model = model_for_cost(&self.partial);
        finalize_usage(&mut self.partial.usage, &cost_model);

        let (stop_reason, done_reason, error_detail) = classify_finish(&self.finish_reason);

        self.partial.stop_reason = stop_reason.clone();

        if let Some(reason) = done_reason {
            return AssistantMessageEvent::Done {
                reason,
                message: self.partial,
            };
        }

        if self.partial.error.is_none() {
            self.partial.error = Some(error_detail.unwrap_or_else(|| {
                AssistantError::new(
                    ErrorCategory::Unknown,
                    format!(
                        "openai-completions: terminated without recognized finish_reason ({:?})",
                        self.finish_reason
                    ),
                )
            }));
        }
        AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: self.partial,
        }
    }
}

/// We finalize cost using only the metadata from the running message;
/// reconstructing a [`ModelInfo`] purely for cost lookup keeps the
/// state machine free of a lifetime tie back to the provider call.
fn model_for_cost(message: &AssistantMessage) -> ModelInfo {
    use crate::registry::{InputModality, ModelCost};
    ModelInfo {
        id: message.model.clone(),
        name: message.model.clone(),
        api: message.api.clone(),
        provider: message.provider.clone(),
        base_url: String::new(),
        reasoning: false,
        supports_xhigh: false,
        supports_adaptive_thinking: false,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 0,
        max_tokens: 0,
        headers: None,
    }
}

/// Map a `finish_reason` to the unified terminal triple.
///
/// Returns `(stop_reason, done_reason, error_detail)`. When
/// `done_reason` is `Some`, the message terminates with `Done`;
/// otherwise it terminates with `Error` and `error_detail` is set.
fn classify_finish(
    finish: &Option<FinishReason>,
) -> (StopReason, Option<DoneReason>, Option<AssistantError>) {
    match finish {
        Some(FinishReason::Stop) => (StopReason::Stop, Some(DoneReason::Stop), None),
        Some(FinishReason::Length) => (StopReason::Length, Some(DoneReason::Length), None),
        Some(FinishReason::ToolCalls) | Some(FinishReason::FunctionCall) => {
            (StopReason::ToolUse, Some(DoneReason::ToolUse), None)
        }
        Some(FinishReason::ContentFilter) => (
            StopReason::Error,
            None,
            Some(classify_openai_finish_reason(
                "content_filter",
                "Provider finish_reason: content_filter".to_string(),
            )),
        ),
        Some(FinishReason::NetworkError) => (
            StopReason::Error,
            None,
            Some(classify_openai_finish_reason(
                "network_error",
                "Provider finish_reason: network_error".to_string(),
            )),
        ),
        Some(FinishReason::Other(other)) => (
            StopReason::Error,
            None,
            Some(classify_openai_finish_reason(
                other,
                format!("Provider finish_reason: {other}"),
            )),
        ),
        None => (StopReason::Stop, Some(DoneReason::Stop), None),
    }
}

// ---------------------------------------------------------------------------
// Usage merging + cost (§7.2 "Usage parsing")
// ---------------------------------------------------------------------------

fn apply_usage(target: &mut Usage, source: &ChatUsage) {
    let cached = source
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .map(u64::from)
        .unwrap_or(0);
    let cache_write = source
        .prompt_tokens_details
        .as_ref()
        .and_then(|d| d.cache_write_tokens)
        .map(u64::from)
        .unwrap_or(0);

    // §7.2: when an OpenAI-compatible provider reports cache_write
    // tokens *inside* the cached_tokens count, subtract them so the
    // result is a pure cache-read figure.
    let cache_read = if cache_write > 0 {
        cached.saturating_sub(cache_write)
    } else {
        cached
    };

    let prompt_tokens = u64::from(source.prompt_tokens);
    let input = prompt_tokens
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);

    target.input = input;
    target.cache_read = cache_read;
    target.cache_write = cache_write;
    // §7.2: completion_tokens already includes reasoning tokens as a
    // subset on native OpenAI; do not add reasoning_tokens separately.
    target.output = u64::from(source.completion_tokens);
}

fn finalize_usage(usage: &mut Usage, model: &ModelInfo) {
    // §7.2: trust our own arithmetic over the wire's `total_tokens`.
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
    calculate_cost(model, usage);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{InputModality, ModelCost};
    use crate::types::{AssistantContent, Message, ThinkingContent, UserContent, UserMessage};
    use openai_sdk::types::chat_completions::{
        ChatCompletionStreamChoice, ChatCompletionStreamResponseDelta, FunctionCallDelta,
        PromptTokensDetails, ToolCallDelta,
    };

    fn fake_model() -> ModelInfo {
        ModelInfo {
            id: "gpt-5".into(),
            name: "GPT-5".into(),
            api: API_NAME.into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning: true,
            supports_xhigh: false,
            supports_adaptive_thinking: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 1.25,
                output: 10.0,
                cache_read: 0.125,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_tokens: 16_000,
            headers: None,
        }
    }

    fn non_reasoning_model() -> ModelInfo {
        ModelInfo {
            id: "gpt-4o".into(),
            name: "GPT-4o".into(),
            reasoning: false,
            ..fake_model()
        }
    }

    // ----- Conversion helpers -----

    #[test]
    fn build_request_uses_developer_role_for_reasoning_models() {
        let context = Context::new("you are helpful");
        let req = build_request(&fake_model(), &context, &StreamOptions::default(), None);
        match &req.messages[0] {
            ChatCompletionRequestMessage::Developer { content, .. } => match content {
                ChatCompletionTextContent::String(s) => assert_eq!(s, "you are helpful"),
                _ => panic!("unexpected content shape"),
            },
            other => panic!("expected developer role, got {other:?}"),
        }
    }

    #[test]
    fn build_request_uses_system_role_for_non_reasoning_models() {
        let context = Context::new("you are helpful");
        let req = build_request(
            &non_reasoning_model(),
            &context,
            &StreamOptions::default(),
            None,
        );
        match &req.messages[0] {
            ChatCompletionRequestMessage::System { content, .. } => match content {
                ChatCompletionTextContent::String(s) => assert_eq!(s, "you are helpful"),
                _ => panic!("unexpected content shape"),
            },
            other => panic!("expected system role, got {other:?}"),
        }
    }

    #[test]
    fn build_request_omits_reasoning_effort_on_non_reasoning_models() {
        let context = Context::new("sys");
        let req = build_request(
            &non_reasoning_model(),
            &context,
            &StreamOptions::default(),
            Some(&ThinkingLevel::High),
        );
        assert!(req.reasoning_effort.is_none());
    }

    #[test]
    fn build_request_sets_store_false_and_include_usage() {
        let context = Context::new("sys");
        let req = build_request(&fake_model(), &context, &StreamOptions::default(), None);
        assert_eq!(req.store, Some(false));
        assert_eq!(
            req.stream_options
                .as_ref()
                .and_then(|s| s.include_usage)
                .unwrap_or(false),
            true
        );
    }

    #[test]
    fn xhigh_falls_back_to_high_when_unsupported() {
        let m = fake_model();
        assert!(matches!(
            map_reasoning_effort(Some(&ThinkingLevel::XHigh), &m),
            ReasoningEffort::High
        ));
        let mut m = m;
        m.supports_xhigh = true;
        assert!(matches!(
            map_reasoning_effort(Some(&ThinkingLevel::XHigh), &m),
            ReasoningEffort::XHigh
        ));
    }

    #[test]
    fn assistant_with_only_thinking_is_dropped_on_outbound() {
        let assistant = AssistantMessage {
            content: vec![AssistantContent::Thinking(ThinkingContent {
                thinking: "private thoughts".into(),
                thinking_signature: None,
                redacted: false,
            })],
            api: API_NAME.into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        };
        let mut out = Vec::new();
        convert_messages(&[Message::Assistant(assistant)], &mut out);
        assert!(out.is_empty(), "thinking-only assistant should be dropped");
    }

    #[test]
    fn tool_result_with_image_emits_followup_user_message() {
        let mut tr = ToolResultMessage::text("call_1", "screenshot", "captured", false);
        tr.content.push(UserContent::image("aGVsbG8=", "image/png"));
        let mut out = Vec::new();
        convert_messages(&[Message::ToolResult(tr)], &mut out);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0], ChatCompletionRequestMessage::Tool { .. }));
        match &out[1] {
            ChatCompletionRequestMessage::User { content, .. } => match content {
                ChatCompletionUserContent::Array(parts) => {
                    assert_eq!(parts.len(), 1);
                    assert!(matches!(
                        parts[0],
                        ChatCompletionUserContentPart::ImageUrl { .. }
                    ));
                }
                _ => panic!("expected array content for image followup"),
            },
            _ => panic!("expected user followup, got {:?}", out[1]),
        }
    }

    #[test]
    fn user_image_serializes_as_data_url() {
        let user = UserMessage {
            content: vec![
                UserContent::text("describe this"),
                UserContent::image("Zm9v", "image/png"),
            ],
            timestamp: 0,
        };
        let m = convert_user_message(&user);
        match m {
            ChatCompletionRequestMessage::User {
                content: ChatCompletionUserContent::Array(parts),
                ..
            } => {
                assert_eq!(parts.len(), 2);
                match &parts[1] {
                    ChatCompletionUserContentPart::ImageUrl { image_url } => {
                        assert!(image_url.url.starts_with("data:image/png;base64,"));
                        assert!(image_url.url.ends_with("Zm9v"));
                    }
                    other => panic!("expected ImageUrl, got {other:?}"),
                }
            }
            other => panic!("expected user message with array content, got {other:?}"),
        }
    }

    #[test]
    fn tool_choice_omitted_when_no_tools() {
        // Even an explicit `Required` should be dropped when there are
        // no tools — the API rejects it otherwise.
        assert!(to_chat_tool_choice(Some(&ToolChoice::Required), false).is_none());
        assert!(to_chat_tool_choice(Some(&ToolChoice::Auto), false).is_none());
        let with_tools = to_chat_tool_choice(Some(&ToolChoice::Required), true).unwrap();
        match with_tools {
            ChatToolChoice::String(s) => assert_eq!(s, "required"),
            _ => panic!("expected string tool choice"),
        }
        let named =
            to_chat_tool_choice(Some(&ToolChoice::Tool { name: "ls".into() }), true).unwrap();
        match named {
            ChatToolChoice::Object { r#type, function } => {
                assert_eq!(r#type, "function");
                assert_eq!(function.name, "ls");
            }
            _ => panic!("expected object tool choice"),
        }
    }

    // ----- Streaming state machine -----

    fn empty_chunk() -> CreateChatCompletionStreamResponse {
        #[allow(deprecated)]
        CreateChatCompletionStreamResponse {
            id: "chatcmpl_1".into(),
            object: "chat.completion.chunk".into(),
            created: 0,
            model: "gpt-5".into(),
            choices: Vec::new(),
            usage: None,
            system_fingerprint: None,
            service_tier: None,
        }
    }

    fn delta_chunk(delta: ChatCompletionStreamResponseDelta) -> CreateChatCompletionStreamResponse {
        let mut chunk = empty_chunk();
        chunk.choices.push(ChatCompletionStreamChoice {
            index: 0,
            delta,
            finish_reason: None,
            logprobs: None,
        });
        chunk
    }

    fn finish_chunk(reason: FinishReason) -> CreateChatCompletionStreamResponse {
        let mut chunk = empty_chunk();
        chunk.choices.push(ChatCompletionStreamChoice {
            index: 0,
            delta: ChatCompletionStreamResponseDelta {
                role: None,
                content: None,
                refusal: None,
                reasoning_content: None,
                tool_calls: Vec::new(),
            },
            finish_reason: Some(reason),
            logprobs: None,
        });
        chunk
    }

    fn text_delta(text: &str) -> ChatCompletionStreamResponseDelta {
        ChatCompletionStreamResponseDelta {
            role: None,
            content: Some(text.into()),
            refusal: None,
            reasoning_content: None,
            tool_calls: Vec::new(),
        }
    }

    fn thinking_delta(text: &str) -> ChatCompletionStreamResponseDelta {
        ChatCompletionStreamResponseDelta {
            role: None,
            content: None,
            refusal: None,
            reasoning_content: Some(text.into()),
            tool_calls: Vec::new(),
        }
    }

    fn tool_call_delta(
        index: i32,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
    ) -> ChatCompletionStreamResponseDelta {
        ChatCompletionStreamResponseDelta {
            role: None,
            content: None,
            refusal: None,
            reasoning_content: None,
            tool_calls: vec![ToolCallDelta {
                index,
                id: id.map(|s| s.to_string()),
                r#type: None,
                function: Some(FunctionCallDelta {
                    name: name.map(|s| s.to_string()),
                    arguments: args.map(|s| s.to_string()),
                }),
                custom: None,
            }],
        }
    }

    #[test]
    fn streamstate_text_pipeline() {
        let mut state = StreamState::new(&fake_model());
        let mut events = Vec::new();
        events.extend(state.process(delta_chunk(text_delta("he"))).events);
        events.extend(state.process(delta_chunk(text_delta("llo"))).events);
        events.extend(state.process(finish_chunk(FinishReason::Stop)).events);

        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        assert!(matches!(events[1], AssistantMessageEvent::TextStart { .. }));
        match &events[2] {
            AssistantMessageEvent::TextDelta { delta, .. } => assert_eq!(delta, "he"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
        // Second chunk only emits the second TextDelta (no second Start).
        match &events[3] {
            AssistantMessageEvent::TextDelta { delta, .. } => assert_eq!(delta, "llo"),
            other => panic!("expected TextDelta, got {other:?}"),
        }
        // finish_reason closes the text block.
        match events.last().unwrap() {
            AssistantMessageEvent::TextEnd { content, .. } => assert_eq!(content, "hello"),
            other => panic!("expected TextEnd, got {other:?}"),
        }

        let final_event = state.finalize();
        match final_event {
            AssistantMessageEvent::Done { reason, message } => {
                assert_eq!(reason, DoneReason::Stop);
                assert_eq!(message.content.len(), 1);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn streamstate_thinking_then_text() {
        let mut state = StreamState::new(&fake_model());
        let mut events = Vec::new();
        events.extend(state.process(delta_chunk(thinking_delta("hmm"))).events);
        events.extend(state.process(delta_chunk(text_delta("answer"))).events);
        events.extend(state.process(finish_chunk(FinishReason::Stop)).events);

        // Order: Start, ThinkingStart, ThinkingDelta, ThinkingEnd,
        // TextStart, TextDelta, TextEnd.
        let kinds: Vec<&str> = events
            .iter()
            .map(|ev| match ev {
                AssistantMessageEvent::Start { .. } => "Start",
                AssistantMessageEvent::ThinkingStart { .. } => "ThinkingStart",
                AssistantMessageEvent::ThinkingDelta { .. } => "ThinkingDelta",
                AssistantMessageEvent::ThinkingEnd { .. } => "ThinkingEnd",
                AssistantMessageEvent::TextStart { .. } => "TextStart",
                AssistantMessageEvent::TextDelta { .. } => "TextDelta",
                AssistantMessageEvent::TextEnd { .. } => "TextEnd",
                _ => "Other",
            })
            .collect();
        assert_eq!(
            kinds,
            vec![
                "Start",
                "ThinkingStart",
                "ThinkingDelta",
                "ThinkingEnd",
                "TextStart",
                "TextDelta",
                "TextEnd",
            ]
        );
    }

    #[test]
    fn streamstate_tool_call_partial_arguments() {
        let mut state = StreamState::new(&fake_model());
        let mut events = Vec::new();
        // First delta: id + name + initial arguments.
        events.extend(
            state
                .process(delta_chunk(tool_call_delta(
                    0,
                    Some("call_abc"),
                    Some("read_file"),
                    Some("{\"path\": "),
                )))
                .events,
        );
        // Second delta: more arguments.
        events.extend(
            state
                .process(delta_chunk(tool_call_delta(
                    0,
                    None,
                    None,
                    Some("\"/tmp/x\"}"),
                )))
                .events,
        );
        // Final: finish_reason closes the call.
        events.extend(state.process(finish_chunk(FinishReason::ToolCalls)).events);

        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        assert!(matches!(
            events
                .iter()
                .find(|e| matches!(e, AssistantMessageEvent::ToolCallStart { .. })),
            Some(_)
        ));
        let end_event = events
            .iter()
            .find(|e| matches!(e, AssistantMessageEvent::ToolCallEnd { .. }))
            .expect("ToolCallEnd present");
        match end_event {
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                assert_eq!(tool_call.id, "call_abc");
                assert_eq!(tool_call.name, "read_file");
                assert_eq!(tool_call.arguments["path"], "/tmp/x");
            }
            _ => unreachable!(),
        }

        let final_event = state.finalize();
        match final_event {
            AssistantMessageEvent::Done { reason, .. } => {
                assert_eq!(reason, DoneReason::ToolUse);
            }
            other => panic!("expected Done(ToolUse), got {other:?}"),
        }
    }

    #[test]
    fn streamstate_finalize_classifies_finish_reasons() {
        for (finish, expect_done) in [
            (FinishReason::Stop, Some(DoneReason::Stop)),
            (FinishReason::Length, Some(DoneReason::Length)),
            (FinishReason::ToolCalls, Some(DoneReason::ToolUse)),
            (FinishReason::FunctionCall, Some(DoneReason::ToolUse)),
        ] {
            let (_, done, _) = classify_finish(&Some(finish.clone()));
            assert_eq!(done, expect_done, "for {finish:?}");
        }

        let (stop, done, err) = classify_finish(&Some(FinishReason::ContentFilter));
        assert_eq!(stop, StopReason::Error);
        assert!(done.is_none());
        let err = err.expect("error detail set");
        assert_eq!(err.category, ErrorCategory::ContentFilter);
        assert!(err.message.contains("content_filter"));

        let (stop, done, err) = classify_finish(&Some(FinishReason::NetworkError));
        assert_eq!(stop, StopReason::Error);
        assert!(done.is_none());
        let err = err.expect("error detail set");
        assert_eq!(err.category, ErrorCategory::Transient);
        assert!(err.message.contains("network_error"));

        let (stop, done, err) = classify_finish(&Some(FinishReason::Other("boom".into())));
        assert_eq!(stop, StopReason::Error);
        assert!(done.is_none());
        let err = err.expect("error detail set");
        assert_eq!(err.category, ErrorCategory::Unknown);
        assert!(err.message.contains("boom"));
    }

    #[test]
    fn streamstate_usage_subtracts_cache_write_from_cached() {
        let mut state = StreamState::new(&fake_model());
        let _ = state.process(delta_chunk(text_delta("hi")));
        let _ = state.process(finish_chunk(FinishReason::Stop));
        // Streaming usage on the final chunk.
        let mut chunk = empty_chunk();
        chunk.usage = Some(ChatUsage {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_tokens_details: Some(PromptTokensDetails {
                audio_tokens: None,
                cached_tokens: Some(40),
                cache_write_tokens: Some(15),
            }),
            completion_tokens_details: None,
        });
        let _ = state.process(chunk);

        let event = state.finalize();
        match event {
            AssistantMessageEvent::Done { message, .. } => {
                assert_eq!(message.usage.cache_write, 15);
                assert_eq!(message.usage.cache_read, 25); // 40 - 15
                assert_eq!(message.usage.input, 60); // 100 - 25 - 15
                assert_eq!(message.usage.output, 20);
                assert_eq!(message.usage.total_tokens, 120);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }

    #[test]
    fn streamstate_usage_pure_cache_read_when_no_cache_write() {
        let mut state = StreamState::new(&fake_model());
        let _ = state.process(delta_chunk(text_delta("hi")));
        let _ = state.process(finish_chunk(FinishReason::Stop));
        let mut chunk = empty_chunk();
        chunk.usage = Some(ChatUsage {
            prompt_tokens: 50,
            completion_tokens: 10,
            total_tokens: 60,
            prompt_tokens_details: Some(PromptTokensDetails {
                audio_tokens: None,
                cached_tokens: Some(30),
                cache_write_tokens: None,
            }),
            completion_tokens_details: None,
        });
        let _ = state.process(chunk);

        match state.finalize() {
            AssistantMessageEvent::Done { message, .. } => {
                assert_eq!(message.usage.cache_read, 30);
                assert_eq!(message.usage.cache_write, 0);
                assert_eq!(message.usage.input, 20);
            }
            other => panic!("expected Done, got {other:?}"),
        }
    }
}
