//! Anthropic Messages API provider.
//!
//! Implements the unified [`Provider`] trait against Anthropic's
//! `POST /v1/messages` streaming endpoint. See `docs/models-spec.md` §6.2.
//!
//! Stateless — per-call HTTP knobs (auth, base URL, betas, caching) are
//! derived from the per-call [`ModelInfo`] and [`StreamOptions`] so the
//! same instance can serve any number of concurrent requests.

use anthropic_sdk::client::{Client, ClientError};
use anthropic_sdk::messages::{
    CacheControl, ContentBlock as AContentBlock, ContentBlockDelta as AContentBlockDelta,
    ContentBlockParam, ImageSource as AImageSource, MessageParam, Messages as AMessages, Metadata,
    OutputConfig, OutputEffort, Role as ARole, ServerSentEvent, StopDetails as AStopDetails,
    StopReason as AStopReason, Thinking as AThinking, ToolChoice as ATC, ToolResultContent as ATRC,
    ToolUnion, Usage as AUsage, UsageDelta as AUsageDelta,
};
use futures::StreamExt;
use serde_json::Value;

use crate::errors::{
    classify_anthropic_error, classify_anthropic_stop_reason, parse_retry_after, transport_error,
};
use crate::partial_json::parse_streaming_json;
use crate::provider::Provider;
use crate::registry::{ModelInfo, calculate_cost, supports_adaptive_thinking, supports_xhigh};
use crate::streaming::{
    AssistantMessageEvent, AssistantMessageEventStream, DoneReason, ErrorReason,
};
use crate::transform::transform_messages;
use crate::types::{
    AssistantContent, AssistantError, AssistantMessage, CacheRetention, Context, ErrorCategory,
    Message, SimpleStreamOptions, StopReason, StreamOptions, TextContent, ThinkingContent,
    ThinkingLevel, ToolCall, ToolChoice, ToolDefinition, ToolResultMessage, Usage, UserContent,
    UserMessage,
};

/// `api` field reported on assistant messages produced by this provider.
const API_NAME: &str = "anthropic-messages";

/// Stateless provider for the Anthropic Messages API.
pub struct AnthropicProvider;

impl Provider for AnthropicProvider {
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
    let api_key = options.api_key.clone().ok_or_else(|| {
        // Missing credentials before any HTTP call: surface as Auth so
        // callers and the agent's retry layer see the right category.
        AssistantError::new(
            ErrorCategory::Auth,
            "anthropic provider requires StreamOptions.api_key",
        )
    })?;

    let client = build_client(model, api_key, reasoning, options);
    let request = build_request(model, context, options, reasoning);

    if let Some(cb) = options.on_payload.as_ref() {
        match serde_json::to_value(&request) {
            Ok(body) => cb.call(&body),
            Err(err) => tracing::warn!("on_payload serialization failed: {err}"),
        }
    }

    let mut sse = client
        .messages_stream_raw(request)
        .await
        .map_err(|err| classify_client_error(&err))?;

    let mut state = StreamState::new(model);
    let mut saw_terminal = false;

    while let Some(event) = sse.next().await {
        let outcome = state.process(event);
        for ev in outcome.events {
            producer.push(ev);
        }
        if outcome.terminal {
            saw_terminal = true;
            break;
        }
    }

    // The SSE stream is expected to deliver `MessageStop` (or `Error`),
    // at which point we synthesize the final `Done` / `Error` event.
    // If the stream closes before that, treat the partial as best-effort.
    if !saw_terminal {
        tracing::debug!("anthropic SSE closed without MessageStop; finalizing partial");
    }
    let final_event = state.finalize();
    producer.push(final_event);
    Ok(())
}

// ---------------------------------------------------------------------------
// Client construction
// ---------------------------------------------------------------------------

fn build_client(
    model: &ModelInfo,
    api_key: String,
    reasoning: Option<&ThinkingLevel>,
    options: &StreamOptions,
) -> Client {
    let base_url = if model.base_url.is_empty() {
        None
    } else {
        Some(model.base_url.clone())
    };
    let mut client = Client::new(base_url, api_key);
    // The interleaved-thinking beta is only valid on non-adaptive
    // reasoning models; adaptive models reject it (Opus 4.7) or treat
    // it as redundant. Send it only when reasoning is on AND the model
    // is not adaptive.
    if reasoning.is_some() && model.reasoning && !supports_adaptive_thinking(model) {
        client = client.with_interleaved_thinking(true);
    }
    for beta in extra_betas_from_headers(options.headers.as_ref()) {
        client = client.with_beta(beta);
    }
    client
}

/// Extract the per-call `anthropic-beta` values out of
/// [`StreamOptions::headers`] so they can be merged into the SDK
/// client's beta list. Comma-separated values are split because that's
/// the wire format the API accepts when callers stuff several betas
/// into a single header value. Matching is case-insensitive and
/// whitespace around each entry is trimmed; empty entries are dropped
/// silently so a stray trailing comma doesn't poison the request.
fn extra_betas_from_headers(
    headers: Option<&std::collections::HashMap<String, String>>,
) -> Vec<String> {
    let Some(headers) = headers else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, value) in headers {
        if !name.eq_ignore_ascii_case("anthropic-beta") {
            continue;
        }
        for beta in value.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            out.push(beta.to_string());
        }
    }
    out
}

/// Classify a transport-layer or SDK-surfaced error into the unified
/// [`AssistantError`] shape per `docs/models-spec.md` §10.3.
fn classify_client_error(err: &ClientError) -> AssistantError {
    match err {
        ClientError::ApiError {
            error,
            http_status,
            retry_after,
        } => classify_anthropic_error(
            Some(error.type_tag()),
            Some(*http_status),
            parse_retry_after(retry_after.as_deref()),
            error.message().to_string(),
        ),
        ClientError::TransportError(t) => transport_error(format!("transport: {t}")),
        ClientError::ParseError(s) => transport_error(format!("parse: {s}")),
        ClientError::InternalError(s) => transport_error(format!("internal: {s}")),
    }
}

// ---------------------------------------------------------------------------
// Request body construction (§6.2)
// ---------------------------------------------------------------------------

fn build_request(
    model: &ModelInfo,
    context: &Context,
    options: &StreamOptions,
    reasoning: Option<&ThinkingLevel>,
) -> AMessages {
    let thinking_enabled = reasoning.is_some() && model.reasoning;

    // §8: rewrite the history for cross-provider replay (signature
    // strip, tool-call ID normalization, orphan/errored handling, image
    // downgrade) before serializing into Anthropic message params.
    let transformed = transform_messages(&context.messages, model);
    let messages = convert_messages(&transformed);
    let messages = apply_request_cache_control(messages, options, model);

    let system = build_system(context.system_prompt.as_deref(), options, model);

    let tools: Vec<ToolUnion> = context.tools.iter().map(to_anthropic_tool).collect();
    let tool_choice = to_anthropic_tool_choice(options.tool_choice.as_ref(), !tools.is_empty());

    // Spec §6.2: default `max_tokens` to model.max_tokens / 3 when the
    // caller doesn't override it. We clamp to at least 1 to keep the API
    // happy on degenerate (zero) catalog values.
    let max_tokens = options
        .max_tokens
        .unwrap_or_else(|| (model.max_tokens / 3).max(1));

    // Anthropic rejects `temperature` when extended thinking is on.
    let temperature = if thinking_enabled {
        None
    } else {
        options.temperature
    };

    let (thinking, output_config) = build_thinking(model, reasoning);

    let metadata = build_metadata(options);

    AMessages {
        model: model.id.clone(),
        messages,
        max_tokens,
        system,
        tools,
        tool_choice,
        thinking,
        output_config,
        temperature,
        metadata,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Message conversion (§6.2 "Message conversion")
// ---------------------------------------------------------------------------

/// Convert the unified message log into Anthropic message params,
/// batching consecutive `ToolResult`s into a single user message.
fn convert_messages(messages: &[Message]) -> Vec<MessageParam> {
    let mut out: Vec<MessageParam> = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg {
            Message::User(u) => out.push(convert_user_message(u)),
            Message::Assistant(a) => out.push(convert_assistant_message(a)),
            Message::ToolResult(tr) => {
                let block = convert_tool_result(tr);
                // Append into a previous all-tool-results user message
                // when possible so multiple sequential results land in
                // the same turn.
                if let Some(last) = out.last_mut()
                    && matches!(last.role, ARole::User)
                    && all_tool_results(&last.content)
                {
                    last.content.push(block);
                    continue;
                }
                out.push(MessageParam {
                    role: ARole::User,
                    content: vec![block],
                });
            }
        }
    }
    out
}

fn all_tool_results(blocks: &[ContentBlockParam]) -> bool {
    !blocks.is_empty()
        && blocks
            .iter()
            .all(|b| matches!(b, ContentBlockParam::ToolResultBlock { .. }))
}

fn convert_user_message(m: &UserMessage) -> MessageParam {
    let content = m.content.iter().map(convert_user_content).collect();
    MessageParam {
        role: ARole::User,
        content,
    }
}

fn convert_user_content(c: &UserContent) -> ContentBlockParam {
    match c {
        UserContent::Text(t) => ContentBlockParam::TextBlock {
            text: t.text.clone(),
            cache_control: None,
            citations: None,
        },
        UserContent::Image(img) => ContentBlockParam::ImageBlock {
            source: AImageSource::Base64 {
                data: img.data.clone(),
                media_type: img.mime_type.clone(),
            },
            cache_control: None,
        },
    }
}

fn convert_assistant_message(m: &AssistantMessage) -> MessageParam {
    let mut content = Vec::with_capacity(m.content.len());
    for block in &m.content {
        match block {
            AssistantContent::Text(t) => content.push(ContentBlockParam::TextBlock {
                text: t.text.clone(),
                cache_control: None,
                citations: None,
            }),
            AssistantContent::Thinking(th) => {
                if th.redacted {
                    // Redacted: signature carries the encrypted payload.
                    if let Some(sig) = th.thinking_signature.as_ref() {
                        content
                            .push(ContentBlockParam::RedactedThinkingBlock { data: sig.clone() });
                    }
                    // No payload to forward when the signature is missing —
                    // safer to drop than to send a malformed block.
                } else if let Some(sig) = th.thinking_signature.as_ref() {
                    content.push(ContentBlockParam::ThinkingBlock {
                        signature: sig.clone(),
                        thinking: th.thinking.clone(),
                    });
                } else if !th.thinking.is_empty() {
                    // Spec §6.2: thinking without a signature (e.g. from
                    // an aborted prior stream) is demoted to plain text on
                    // outgoing requests so the model still has the
                    // context.
                    content.push(ContentBlockParam::TextBlock {
                        text: th.thinking.clone(),
                        cache_control: None,
                        citations: None,
                    });
                }
            }
            AssistantContent::ToolCall(tc) => content.push(ContentBlockParam::ToolUseBlock {
                id: tc.id.clone(),
                input: tc.arguments.clone(),
                name: tc.name.clone(),
                cache_control: None,
                caller: None,
            }),
        }
    }
    MessageParam {
        role: ARole::Assistant,
        content,
    }
}

// ---------------------------------------------------------------------------
// Public round-trip helpers (`docs/models-spec.md` §1.10, §12 step 11b)
// ---------------------------------------------------------------------------

/// Project an [`AssistantMessage`] onto the Anthropic Messages request item
/// shape — the [`MessageParam`] with `role: "assistant"` that gets sent as
/// part of `messages[]` on a follow-up turn.
///
/// This is the serialize side of the §1.10 round-trip invariant. It is the
/// same projection the provider uses internally when building a request
/// body, exposed publicly so the round-trip test suite (and any future
/// consumer that wants to materialize a single assistant turn into its
/// wire form without spinning up a full request) can call it directly.
///
/// Behaviour matches the rules in `docs/models-spec.md` §6.2:
/// - Text blocks are forwarded verbatim.
/// - Thinking blocks with a signature ride as `thinking` blocks.
/// - Redacted thinking blocks ride as `redacted_thinking` with the
///   encrypted payload pulled from `thinking_signature`.
/// - Thinking blocks without a signature (e.g. from an aborted prior
///   stream) are demoted to plain text so the model still sees the
///   context.
/// - Tool calls ride as `tool_use` blocks.
pub fn assistant_message_to_request_item(message: &AssistantMessage) -> MessageParam {
    convert_assistant_message(message)
}

/// Inverse of [`assistant_message_to_request_item`]: parse an Anthropic
/// `messages[]` entry whose role is `assistant` back into a unified
/// [`AssistantMessage`].
///
/// This is the parse side of the §1.10 round-trip invariant — symmetric to
/// the SSE state machine in [`StreamState`], because Anthropic's request
/// and response content blocks share shapes one-for-one. See
/// `docs/models-spec.md` §6.2 for the field mapping; the spec rules
/// preserved here are:
///
/// - `text` → [`AssistantContent::Text`].
/// - `thinking` (with signature) → [`AssistantContent::Thinking`] with
///   `redacted == false` and the signature populated.
/// - `redacted_thinking` → [`AssistantContent::Thinking`] with
///   `redacted == true`, empty visible text, and the encrypted payload
///   in `thinking_signature`.
/// - `tool_use` → [`AssistantContent::ToolCall`].
///
/// Server-only block kinds (server-side tool use, MCP, code execution,
/// search results, citations, …) are not representable in the unified
/// content set and are dropped — matching the streaming parser's
/// `BlockState::Ignored` behaviour. The `role` is taken on faith; passing
/// in a user-role param yields an empty assistant message.
pub fn parse_assistant_request_item(param: &MessageParam) -> AssistantMessage {
    let mut content = Vec::with_capacity(param.content.len());
    for block in &param.content {
        match block {
            ContentBlockParam::TextBlock { text, .. } => {
                content.push(AssistantContent::Text(TextContent {
                    text: text.clone(),
                    text_signature: None,
                }));
            }
            ContentBlockParam::ThinkingBlock {
                signature,
                thinking,
            } => {
                content.push(AssistantContent::Thinking(ThinkingContent {
                    thinking: thinking.clone(),
                    thinking_signature: if signature.is_empty() {
                        None
                    } else {
                        Some(signature.clone())
                    },
                    redacted: false,
                }));
            }
            ContentBlockParam::RedactedThinkingBlock { data } => {
                content.push(AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some(data.clone()),
                    redacted: true,
                }));
            }
            ContentBlockParam::ToolUseBlock {
                id, input, name, ..
            } => {
                content.push(AssistantContent::ToolCall(ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: input.clone(),
                }));
            }
            // Everything else (image / document / search result on the
            // user side, server tool use, MCP, container upload, …) is
            // not part of the unified assistant content set and is
            // silently dropped, matching the streaming parser.
            _ => {}
        }
    }
    let mut out = AssistantMessage::empty();
    out.api = API_NAME.to_string();
    out.content = content;
    out
}

/// Replay a sequence of pre-decoded Anthropic [`ServerSentEvent`]s through
/// the provider's streaming state machine and return the finalized
/// [`AssistantMessage`].
///
/// The fixture-based round-trip tests in `tests/roundtrip/` use this to
/// turn captured SSE wire dumps into unified messages without spinning up
/// a real HTTP client. Provided publicly so external test suites can
/// share the exact same parse path the live provider does.
pub fn replay_sse_events(
    model: &ModelInfo,
    events: impl IntoIterator<Item = ServerSentEvent>,
) -> AssistantMessage {
    let mut state = StreamState::new(model);
    let mut last_event: Option<AssistantMessageEvent> = None;
    for ev in events {
        let outcome = state.process(ev);
        if let Some(last) = outcome.events.into_iter().last() {
            last_event = Some(last);
        }
        if outcome.terminal {
            // `MessageStop` produces no events itself; finalize below.
            // `Error` already produced an `Error` event with the final
            // message; keep that.
            break;
        }
    }
    // If the stream emitted its own terminal `Error` event, prefer that
    // message — it carries the populated `error` and `stop_reason`.
    if let Some(AssistantMessageEvent::Error { error, .. }) = last_event {
        return error;
    }
    match state.finalize() {
        AssistantMessageEvent::Done { message, .. }
        | AssistantMessageEvent::Error { error: message, .. } => message,
        other => panic!("StreamState::finalize returned non-terminal event: {other:?}"),
    }
}

fn convert_tool_result(t: &ToolResultMessage) -> ContentBlockParam {
    // Pure-text results take the cheaper `Text(String)` shape; anything
    // else (multiple blocks, images) goes through the array form.
    let content = if t.content.len() == 1 {
        match &t.content[0] {
            UserContent::Text(text) => ATRC::Text(text.text.clone()),
            UserContent::Image(_) => {
                ATRC::Blocks(t.content.iter().map(convert_user_content).collect())
            }
        }
    } else {
        ATRC::Blocks(t.content.iter().map(convert_user_content).collect())
    };
    ContentBlockParam::ToolResultBlock {
        tool_use_id: t.tool_call_id.clone(),
        cache_control: None,
        content,
        is_error: t.is_error,
    }
}

// ---------------------------------------------------------------------------
// System prompt + cache control (§6.2 "Prompt caching")
// ---------------------------------------------------------------------------

fn build_system(
    system_prompt: Option<&str>,
    options: &StreamOptions,
    model: &ModelInfo,
) -> Option<Vec<ContentBlockParam>> {
    let prompt = system_prompt?;
    if prompt.is_empty() {
        return None;
    }
    let cache_control = cache_control_for(&options.cache_retention, model);
    Some(vec![ContentBlockParam::TextBlock {
        text: prompt.to_string(),
        cache_control,
        citations: None,
    }])
}

fn cache_control_for(retention: &CacheRetention, model: &ModelInfo) -> Option<CacheControl> {
    match retention {
        CacheRetention::None => None,
        CacheRetention::Short => Some(CacheControl::Ephemeral { ttl: None }),
        CacheRetention::Long => {
            // The 1h TTL is direct-API-only; proxies (Bedrock/Vertex)
            // may reject the field, so fall back to the default 5m
            // ephemeral when we're not pointed at api.anthropic.com.
            if model.base_url.contains("api.anthropic.com") {
                Some(CacheControl::Ephemeral {
                    ttl: Some("1h".to_string()),
                })
            } else {
                Some(CacheControl::Ephemeral { ttl: None })
            }
        }
    }
}

/// Tag the last content block of the last user message with cache_control
/// (per spec §6.2). The system prompt's cache marker is set in
/// [`build_system`].
fn apply_request_cache_control(
    mut messages: Vec<MessageParam>,
    options: &StreamOptions,
    model: &ModelInfo,
) -> Vec<MessageParam> {
    let Some(cc) = cache_control_for(&options.cache_retention, model) else {
        return messages;
    };
    if let Some(last_user) = messages
        .iter_mut()
        .rev()
        .find(|m| matches!(m.role, ARole::User))
        && let Some(last_block) = last_user.content.last_mut()
    {
        last_block.set_cache_control(cc);
    }
    messages
}

// ---------------------------------------------------------------------------
// Tools / tool choice (§6.2 "Tool choice mapping")
// ---------------------------------------------------------------------------

fn to_anthropic_tool(tool: &ToolDefinition) -> ToolUnion {
    ToolUnion::Custom {
        name: tool.name.clone(),
        description: Some(tool.description.clone()),
        input_schema: tool.parameters.clone(),
        cache_control: None,
        allowed_callers: Vec::new(),
        defer_loading: None,
        eager_input_streaming: None,
        input_examples: Vec::new(),
        strict: None,
    }
}

fn to_anthropic_tool_choice(choice: Option<&ToolChoice>, has_tools: bool) -> Option<ATC> {
    match choice {
        None => None,
        Some(ToolChoice::Auto) => Some(ATC::Auto {
            disable_parallel_tool_use: None,
        }),
        Some(ToolChoice::Required) => Some(ATC::Any {
            disable_parallel_tool_use: None,
        }),
        Some(ToolChoice::Tool { name }) => Some(ATC::Tool {
            name: name.clone(),
            disable_parallel_tool_use: None,
        }),
        // Per spec §6.2: omit `tool_choice` entirely when no tools are
        // defined; the API would reject `{type: "none"}` with no tools.
        Some(ToolChoice::None) => has_tools.then_some(ATC::None),
    }
}

// ---------------------------------------------------------------------------
// Thinking config (§6.2 "Thinking/reasoning configuration")
// ---------------------------------------------------------------------------

fn build_thinking(
    model: &ModelInfo,
    reasoning: Option<&ThinkingLevel>,
) -> (Option<AThinking>, Option<OutputConfig>) {
    let Some(level) = reasoning else {
        return (Some(AThinking::Disabled), None);
    };
    if !model.reasoning {
        // The caller asked for reasoning on a non-reasoning model.
        // The spec maps this to "disabled" — silently ignoring the
        // ThinkingLevel rather than rejecting the request.
        return (Some(AThinking::Disabled), None);
    }
    if supports_adaptive_thinking(model) {
        let effort = adaptive_effort_for(level, model);
        (
            Some(AThinking::Adaptive { display: None }),
            Some(OutputConfig {
                effort: Some(effort),
                format: None,
                task_budget: None,
            }),
        )
    } else {
        (
            Some(AThinking::Enabled {
                budget_tokens: budget_for(level),
                display: None,
            }),
            None,
        )
    }
}

fn adaptive_effort_for(level: &ThinkingLevel, model: &ModelInfo) -> OutputEffort {
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => OutputEffort::Low,
        ThinkingLevel::Medium => OutputEffort::Medium,
        ThinkingLevel::High => OutputEffort::High,
        ThinkingLevel::XHigh => {
            if supports_xhigh(model) {
                OutputEffort::Max
            } else {
                OutputEffort::High
            }
        }
    }
}

fn budget_for(level: &ThinkingLevel) -> u64 {
    match level {
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        // XHigh has no separate budget tier; falls back to High.
        ThinkingLevel::High | ThinkingLevel::XHigh => 16_384,
    }
}

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

fn build_metadata(options: &StreamOptions) -> Option<Metadata> {
    let metadata = options.metadata.as_ref()?;
    // Anthropic's request `metadata` only models a `user_id` field.
    // Pull it out by name; everything else is ignored.
    let user_id = metadata
        .get("user_id")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    user_id.as_ref()?;
    Some(Metadata { user_id })
}

// ---------------------------------------------------------------------------
// SSE → AssistantMessageEvent state machine (§6.2 "Stream event mapping")
// ---------------------------------------------------------------------------

/// Per-block kind, used to route content_block_delta events to the right
/// running snapshot field. Indexed by Anthropic's `content_block_start.index`.
#[derive(Clone, Debug)]
enum BlockState {
    Text,
    Thinking,
    RedactedThinking,
    /// Tool call block; tracks the cumulative input JSON bytes as they
    /// arrive over `input_json_delta` events.
    ToolCall {
        id: String,
        name: String,
        json: String,
    },
    /// Anything else (server tool use, MCP, search results) — not
    /// representable in the unified content types, so we drop the
    /// deltas silently while still occupying the index slot.
    Ignored,
}

struct StreamState {
    model: ModelInfo,
    /// Running snapshot of the assistant message. Cloned into every
    /// emitted event.
    partial: AssistantMessage,
    /// Per-content-block routing state.
    blocks: Vec<BlockState>,
    /// Latest `stop_reason` seen on a `message_delta`. Used to pick the
    /// terminal event when `message_stop` arrives.
    stop_reason: Option<AStopReason>,
    /// Captured refusal text from a `stop_details: refusal` payload.
    refusal_message: Option<String>,
}

/// Result of processing a single SSE event.
struct ProcessOutcome {
    events: Vec<AssistantMessageEvent>,
    /// Whether the SSE stream has terminated (a `message_stop` or `error`
    /// event has been seen).
    terminal: bool,
}

impl StreamState {
    fn new(model: &ModelInfo) -> Self {
        let mut partial = AssistantMessage::empty();
        partial.api = API_NAME.to_string();
        partial.provider = model.provider.clone();
        partial.model = model.id.clone();
        Self {
            model: model.clone(),
            partial,
            blocks: Vec::new(),
            stop_reason: None,
            refusal_message: None,
        }
    }

    fn process(&mut self, event: ServerSentEvent) -> ProcessOutcome {
        let mut events: Vec<AssistantMessageEvent> = Vec::new();
        let mut terminal = false;

        match event {
            ServerSentEvent::MessageStart { message } => {
                self.partial.response_id = Some(message.id);
                // The server may report a slightly different `model`
                // than the registry id (e.g. version-pinned). Trust
                // the wire value.
                if !message.model.is_empty() {
                    self.partial.model = message.model;
                }
                self.partial.usage = into_unified_usage(&message.usage);
                events.push(AssistantMessageEvent::Start {
                    partial: self.partial.clone(),
                });
            }
            ServerSentEvent::ContentBlockStart {
                index,
                content_block,
            } => {
                let content_index =
                    usize::try_from(index).expect("content_block_start.index fits in usize");
                self.pad_blocks_to(content_index);
                match content_block {
                    AContentBlock::TextBlock { text, .. } => {
                        self.partial
                            .content
                            .push(AssistantContent::Text(TextContent {
                                text,
                                text_signature: None,
                            }));
                        self.blocks.push(BlockState::Text);
                        events.push(AssistantMessageEvent::TextStart {
                            content_index,
                            partial: self.partial.clone(),
                        });
                    }
                    AContentBlock::ThinkingBlock {
                        signature,
                        thinking,
                    } => {
                        self.partial
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent {
                                thinking,
                                thinking_signature: if signature.is_empty() {
                                    None
                                } else {
                                    Some(signature)
                                },
                                redacted: false,
                            }));
                        self.blocks.push(BlockState::Thinking);
                        events.push(AssistantMessageEvent::ThinkingStart {
                            content_index,
                            partial: self.partial.clone(),
                        });
                    }
                    AContentBlock::RedactedThinkingBlock { data } => {
                        self.partial
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: Some(data),
                                redacted: true,
                            }));
                        self.blocks.push(BlockState::RedactedThinking);
                        events.push(AssistantMessageEvent::ThinkingStart {
                            content_index,
                            partial: self.partial.clone(),
                        });
                    }
                    AContentBlock::ToolUseBlock { id, name, .. } => {
                        self.partial
                            .content
                            .push(AssistantContent::ToolCall(ToolCall {
                                id: id.clone(),
                                name: name.clone(),
                                arguments: Value::Object(serde_json::Map::new()),
                            }));
                        self.blocks.push(BlockState::ToolCall {
                            id,
                            name,
                            json: String::new(),
                        });
                        events.push(AssistantMessageEvent::ToolCallStart {
                            content_index,
                            partial: self.partial.clone(),
                        });
                    }
                    _ => {
                        // Unhandled block kinds (server tools, MCP,
                        // citations-only, compaction). Keep the slot
                        // populated so subsequent indices line up.
                        self.partial.content.push(AssistantContent::text(""));
                        self.blocks.push(BlockState::Ignored);
                    }
                }
            }
            ServerSentEvent::ContentBlockDelta { index, delta } => {
                let content_index =
                    usize::try_from(index).expect("content_block_delta.index fits in usize");
                let block = match self.blocks.get_mut(content_index) {
                    Some(b) => b,
                    None => return ProcessOutcome { events, terminal },
                };
                match (block, delta) {
                    (BlockState::Text, AContentBlockDelta::TextDelta { text }) => {
                        if let Some(AssistantContent::Text(t)) =
                            self.partial.content.get_mut(content_index)
                        {
                            t.text.push_str(&text);
                        }
                        events.push(AssistantMessageEvent::TextDelta {
                            content_index,
                            delta: text,
                            partial: self.partial.clone(),
                        });
                    }
                    (BlockState::Thinking, AContentBlockDelta::ThinkingDelta { thinking }) => {
                        if let Some(AssistantContent::Thinking(t)) =
                            self.partial.content.get_mut(content_index)
                        {
                            t.thinking.push_str(&thinking);
                        }
                        events.push(AssistantMessageEvent::ThinkingDelta {
                            content_index,
                            delta: thinking,
                            partial: self.partial.clone(),
                        });
                    }
                    (BlockState::Thinking, AContentBlockDelta::SignatureDelta { signature }) => {
                        if let Some(AssistantContent::Thinking(t)) =
                            self.partial.content.get_mut(content_index)
                        {
                            let sig = t.thinking_signature.get_or_insert_with(String::new);
                            sig.push_str(&signature);
                        }
                        // Signature accumulation is silent — clients
                        // observe the final value via the `partial`
                        // snapshot on the next event.
                    }
                    (
                        BlockState::ToolCall { json, .. },
                        AContentBlockDelta::InputJsonDelta { partial_json },
                    ) => {
                        json.push_str(&partial_json);
                        let parsed = parse_streaming_json(json);
                        if let Some(AssistantContent::ToolCall(tc)) =
                            self.partial.content.get_mut(content_index)
                        {
                            tc.arguments = parsed;
                        }
                        events.push(AssistantMessageEvent::ToolCallDelta {
                            content_index,
                            delta: partial_json,
                            partial: self.partial.clone(),
                        });
                    }
                    _ => {
                        // Citations / compaction / mismatched delta types
                        // for ignored blocks. Drop silently.
                    }
                }
            }
            ServerSentEvent::ContentBlockStop { index } => {
                let content_index =
                    usize::try_from(index).expect("content_block_stop.index fits in usize");
                let Some(block) = self.blocks.get(content_index).cloned() else {
                    return ProcessOutcome { events, terminal };
                };
                match block {
                    BlockState::Text => {
                        let text = match self.partial.content.get(content_index) {
                            Some(AssistantContent::Text(t)) => t.text.clone(),
                            _ => String::new(),
                        };
                        events.push(AssistantMessageEvent::TextEnd {
                            content_index,
                            content: text,
                            partial: self.partial.clone(),
                        });
                    }
                    BlockState::Thinking | BlockState::RedactedThinking => {
                        let content = match self.partial.content.get(content_index) {
                            Some(AssistantContent::Thinking(t)) => t.thinking.clone(),
                            _ => String::new(),
                        };
                        events.push(AssistantMessageEvent::ThinkingEnd {
                            content_index,
                            content,
                            partial: self.partial.clone(),
                        });
                    }
                    BlockState::ToolCall { id, name, json } => {
                        // Final, definitive parse: best-effort partial
                        // parser already starts with strict JSON and
                        // escalates through repair / completion before
                        // falling back to an empty object.
                        let parsed: Value = parse_streaming_json(&json);
                        let tool_call = ToolCall {
                            id,
                            name,
                            arguments: parsed.clone(),
                        };
                        if let Some(AssistantContent::ToolCall(tc)) =
                            self.partial.content.get_mut(content_index)
                        {
                            tc.arguments = parsed;
                        }
                        events.push(AssistantMessageEvent::ToolCallEnd {
                            content_index,
                            tool_call,
                            partial: self.partial.clone(),
                        });
                    }
                    BlockState::Ignored => {}
                }
            }
            ServerSentEvent::MessageDelta {
                delta,
                usage,
                context_management: _,
            } => {
                apply_usage_delta(&mut self.partial.usage, &usage);
                if delta.stop_reason.is_some() {
                    self.stop_reason = delta.stop_reason;
                }
                if let Some(AStopDetails::Refusal {
                    category,
                    explanation,
                }) = &delta.stop_details
                {
                    let category = category.as_deref().unwrap_or("unspecified");
                    let explanation = explanation.as_deref().unwrap_or("(no explanation)");
                    self.refusal_message = Some(format!("refusal ({category}): {explanation}"));
                }
            }
            ServerSentEvent::MessageStop => {
                terminal = true;
            }
            ServerSentEvent::Error { error } => {
                // Mid-stream error events from Anthropic don't carry an
                // HTTP status — they arrive as SSE frames after the
                // 200 OK response. Classify by the typed tag alone.
                self.partial.error = Some(classify_anthropic_error(
                    Some(error.type_tag()),
                    None,
                    None,
                    error.message().to_string(),
                ));
                self.partial.stop_reason = StopReason::Error;
                events.push(AssistantMessageEvent::Error {
                    reason: ErrorReason::Error,
                    error: self.partial.clone(),
                });
                terminal = true;
            }
            ServerSentEvent::Ping => {}
        }

        ProcessOutcome { events, terminal }
    }

    /// Build the terminal event that wraps up the stream. Called
    /// whenever the SSE producer terminates without already having
    /// emitted an `Error` event (which is its own terminator).
    fn finalize(mut self) -> AssistantMessageEvent {
        // Compute total tokens + usage cost on the running message.
        finalize_usage(&mut self.partial.usage, &self.model);

        let (stop_reason, done_reason) = match self.stop_reason {
            Some(AStopReason::EndTurn)
            | Some(AStopReason::PauseTurn)
            | Some(AStopReason::StopSequence)
            | None => (StopReason::Stop, Some(DoneReason::Stop)),
            Some(AStopReason::MaxTokens) => (StopReason::Length, Some(DoneReason::Length)),
            Some(AStopReason::ToolUse) => (StopReason::ToolUse, Some(DoneReason::ToolUse)),
            // Refusal / sensitive / context-window exceeded / compaction
            // are surfaced as errors, matching the unified spec which
            // restricts `Done` to Stop / Length / ToolUse.
            Some(AStopReason::Refusal) => (StopReason::Error, None),
            Some(AStopReason::ModelContextWindowExceeded) => (StopReason::Error, None),
            Some(AStopReason::Compaction) => (StopReason::Error, None),
        };

        self.partial.stop_reason = stop_reason.clone();

        if let Some(reason) = done_reason {
            return AssistantMessageEvent::Done {
                reason,
                message: self.partial,
            };
        }

        // Error-flavored terminal (e.g. refusal). Backfill an error
        // message if we don't already have one — callers should never
        // see a `StopReason::Error` without an accompanying message.
        // Error-flavored terminal (e.g. refusal). Backfill a structured
        // error if we don't already have one — callers should never
        // see a `StopReason::Error` without an accompanying detail.
        if self.partial.error.is_none() {
            let stop_label = match self.stop_reason {
                Some(AStopReason::Refusal) => "refusal",
                Some(AStopReason::ModelContextWindowExceeded) => "model_context_window_exceeded",
                Some(AStopReason::Compaction) => "compaction",
                _ => "unknown",
            };
            let message = self
                .refusal_message
                .clone()
                .unwrap_or_else(|| format!("anthropic stop reason: {:?}", self.stop_reason));
            self.partial.error = Some(classify_anthropic_stop_reason(stop_label, message));
        }
        AssistantMessageEvent::Error {
            reason: ErrorReason::Error,
            error: self.partial,
        }
    }

    /// Pad the per-block state vector and the partial content so that
    /// pushing at slot `index` keeps `blocks.len() == partial.content.len() == index`.
    /// Defensive — Anthropic always emits content blocks in order, but
    /// we don't want to panic on a stray skipped index.
    fn pad_blocks_to(&mut self, index: usize) {
        while self.blocks.len() < index {
            self.blocks.push(BlockState::Ignored);
            self.partial.content.push(AssistantContent::text(""));
        }
    }
}

// ---------------------------------------------------------------------------
// Usage merging + cost (§6.2 "Usage merging")
// ---------------------------------------------------------------------------

fn into_unified_usage(au: &AUsage) -> Usage {
    Usage {
        input: au.input_tokens,
        output: au.output_tokens,
        cache_read: au.cache_read_input_tokens.unwrap_or(0),
        cache_write: au.cache_creation_input_tokens.unwrap_or(0),
        // Anthropic doesn't supply a total; we compute it at finalize.
        total_tokens: 0,
        cost: Default::default(),
    }
}

fn apply_usage_delta(usage: &mut Usage, delta: &AUsageDelta) {
    // `output_tokens` is non-optional on the wire; always update.
    usage.output = delta.output_tokens;
    if let Some(t) = delta.input_tokens {
        usage.input = t;
    }
    if let Some(t) = delta.cache_read_input_tokens {
        usage.cache_read = t;
    }
    if let Some(t) = delta.cache_creation_input_tokens {
        usage.cache_write = t;
    }
}

fn finalize_usage(usage: &mut Usage, model: &ModelInfo) {
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
    use crate::types::{
        AssistantContent, Message, ThinkingContent, ToolCall, UserContent, UserMessage,
    };
    use anthropic_sdk::messages::{Message as AMessage, MessageDelta, MessageType};

    fn fake_model() -> ModelInfo {
        ModelInfo {
            id: "claude-sonnet-4".into(),
            name: "Claude Sonnet 4".into(),
            api: API_NAME.into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            supports_xhigh: false,
            supports_adaptive_thinking: true,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200_000,
            max_tokens: 64_000,
            headers: None,
        }
    }

    fn budget_model() -> ModelInfo {
        ModelInfo {
            supports_adaptive_thinking: false,
            supports_xhigh: false,
            ..fake_model()
        }
    }

    // ----- Conversion helpers -----

    #[test]
    fn convert_user_message_text_and_image() {
        let m = UserMessage {
            content: vec![
                UserContent::text("hello"),
                UserContent::image("Zm9v", "image/png"),
            ],
            timestamp: 0,
        };
        let p = convert_user_message(&m);
        assert!(matches!(p.role, ARole::User));
        assert_eq!(p.content.len(), 2);
        assert!(matches!(p.content[0], ContentBlockParam::TextBlock { .. }));
        assert!(matches!(p.content[1], ContentBlockParam::ImageBlock { .. }));
    }

    #[test]
    fn convert_assistant_thinking_variants() {
        let assistant = AssistantMessage {
            content: vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "with sig".into(),
                    thinking_signature: Some("sig".into()),
                    redacted: false,
                }),
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "no sig".into(),
                    thinking_signature: None,
                    redacted: false,
                }),
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: Some("blob".into()),
                    redacted: true,
                }),
            ],
            api: API_NAME.into(),
            provider: "anthropic".into(),
            model: "x".into(),
            response_id: None,
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error: None,
            timestamp: 0,
        };
        let p = convert_assistant_message(&assistant);
        assert_eq!(p.content.len(), 3);
        assert!(matches!(
            p.content[0],
            ContentBlockParam::ThinkingBlock { .. }
        ));
        // No-signature thinking demoted to plain text.
        assert!(matches!(p.content[1], ContentBlockParam::TextBlock { .. }));
        assert!(matches!(
            p.content[2],
            ContentBlockParam::RedactedThinkingBlock { .. }
        ));
    }

    #[test]
    fn batch_consecutive_tool_results() {
        let messages = vec![
            Message::Assistant(AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "1".into(),
                    name: "a".into(),
                    arguments: serde_json::json!({}),
                })],
                api: API_NAME.into(),
                provider: "anthropic".into(),
                model: "x".into(),
                response_id: None,
                usage: Default::default(),
                stop_reason: StopReason::ToolUse,
                error: None,
                timestamp: 0,
            }),
            Message::ToolResult(ToolResultMessage::text("1", "a", "ra", false)),
            Message::ToolResult(ToolResultMessage::text("2", "b", "rb", false)),
            Message::User(UserMessage::text("done")),
        ];
        let out = convert_messages(&messages);
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0].role, ARole::Assistant));
        assert!(matches!(out[1].role, ARole::User));
        assert_eq!(out[1].content.len(), 2);
        assert!(matches!(
            out[1].content[0],
            ContentBlockParam::ToolResultBlock { .. }
        ));
        assert!(matches!(out[2].role, ARole::User));
    }

    #[test]
    fn tool_choice_omitted_when_none_with_no_tools() {
        let auto = to_anthropic_tool_choice(Some(&ToolChoice::Auto), false);
        assert!(matches!(auto, Some(ATC::Auto { .. })));

        let none_no_tools = to_anthropic_tool_choice(Some(&ToolChoice::None), false);
        assert!(none_no_tools.is_none());

        let none_with_tools = to_anthropic_tool_choice(Some(&ToolChoice::None), true);
        assert!(matches!(none_with_tools, Some(ATC::None)));

        let required = to_anthropic_tool_choice(Some(&ToolChoice::Required), true);
        assert!(matches!(required, Some(ATC::Any { .. })));

        let named = to_anthropic_tool_choice(Some(&ToolChoice::Tool { name: "ls".into() }), true);
        assert!(matches!(named, Some(ATC::Tool { ref name, .. }) if name == "ls"));
    }

    #[test]
    fn cache_control_long_falls_back_off_anthropic_host() {
        let mut model = fake_model();
        model.base_url = "https://bedrock.example/anthropic".into();
        let cc = cache_control_for(&CacheRetention::Long, &model).unwrap();
        match cc {
            CacheControl::Ephemeral { ttl } => assert!(ttl.is_none()),
        }
        let cc = cache_control_for(&CacheRetention::Long, &fake_model()).unwrap();
        match cc {
            CacheControl::Ephemeral { ttl } => assert_eq!(ttl.as_deref(), Some("1h")),
        }
        assert!(cache_control_for(&CacheRetention::None, &fake_model()).is_none());
    }

    #[test]
    fn build_thinking_adaptive_vs_budget() {
        let (think, oc) = build_thinking(&fake_model(), Some(&ThinkingLevel::High));
        assert!(matches!(think, Some(AThinking::Adaptive { .. })));
        let oc = oc.unwrap();
        assert!(matches!(oc.effort, Some(OutputEffort::High)));

        let (think, oc) = build_thinking(&budget_model(), Some(&ThinkingLevel::Medium));
        assert!(matches!(
            think,
            Some(AThinking::Enabled {
                budget_tokens: 8192,
                ..
            })
        ));
        assert!(oc.is_none());

        // No reasoning + reasoning-capable model → disabled.
        let (think, oc) = build_thinking(&fake_model(), None);
        assert!(matches!(think, Some(AThinking::Disabled)));
        assert!(oc.is_none());
    }

    #[test]
    fn xhigh_falls_back_to_high_when_unsupported() {
        let mut m = fake_model();
        m.supports_xhigh = false;
        let (_think, oc) = build_thinking(&m, Some(&ThinkingLevel::XHigh));
        assert!(matches!(oc.unwrap().effort, Some(OutputEffort::High)));

        m.supports_xhigh = true;
        let (_think, oc) = build_thinking(&m, Some(&ThinkingLevel::XHigh));
        assert!(matches!(oc.unwrap().effort, Some(OutputEffort::Max)));
    }

    #[test]
    fn build_request_omits_temperature_when_thinking() {
        let model = fake_model();
        let context = Context::new("sys");
        let mut options = StreamOptions::default();
        options.temperature = Some(0.7);
        let req = build_request(&model, &context, &options, Some(&ThinkingLevel::High));
        assert!(req.temperature.is_none());
        let req = build_request(&model, &context, &options, None);
        assert_eq!(req.temperature, Some(0.7));
    }

    #[test]
    fn build_request_default_max_tokens_is_third() {
        let model = fake_model();
        let context = Context::new("sys");
        let options = StreamOptions::default();
        let req = build_request(&model, &context, &options, None);
        assert_eq!(req.max_tokens, model.max_tokens / 3);
    }

    #[test]
    fn build_request_marks_cache_control_on_last_user() {
        let mut context = Context::new("sys");
        context
            .messages
            .push(Message::User(UserMessage::text("u1")));
        context
            .messages
            .push(Message::User(UserMessage::text("u2")));
        let req = build_request(&fake_model(), &context, &StreamOptions::default(), None);
        // last user is at index 1, last block at index 0.
        let last = req.messages.last().unwrap();
        let cc = last.content.last().unwrap();
        match cc {
            ContentBlockParam::TextBlock { cache_control, .. } => {
                assert!(cache_control.is_some());
            }
            _ => panic!("unexpected block"),
        }
        // System prompt also carries cache_control.
        let sys = req.system.unwrap();
        match &sys[0] {
            ContentBlockParam::TextBlock { cache_control, .. } => {
                assert!(cache_control.is_some());
            }
            _ => panic!(),
        }
    }

    // ----- Streaming state machine -----

    fn empty_a_message() -> AMessage {
        AMessage {
            id: "msg_1".into(),
            r#type: MessageType::Message,
            role: ARole::Assistant,
            content: Vec::new(),
            model: "claude-sonnet-4-20250514".into(),
            stop_reason: None,
            stop_sequence: None,
            stop_details: None,
            usage: AUsage {
                input_tokens: 12,
                output_tokens: 0,
                cache_creation_input_tokens: Some(2),
                cache_read_input_tokens: Some(4),
                ..Default::default()
            },
            container: None,
            context_management: None,
        }
    }

    #[test]
    fn streamstate_emits_text_pipeline() {
        let mut state = StreamState::new(&fake_model());
        let mut events = Vec::new();
        for ev in state
            .process(ServerSentEvent::MessageStart {
                message: empty_a_message(),
            })
            .events
        {
            events.push(ev);
        }
        for ev in state
            .process(ServerSentEvent::ContentBlockStart {
                index: 0,
                content_block: AContentBlock::TextBlock {
                    text: String::new(),
                    citations: Vec::new(),
                },
            })
            .events
        {
            events.push(ev);
        }
        for ev in state
            .process(ServerSentEvent::ContentBlockDelta {
                index: 0,
                delta: AContentBlockDelta::TextDelta { text: "hi".into() },
            })
            .events
        {
            events.push(ev);
        }
        for ev in state
            .process(ServerSentEvent::ContentBlockStop { index: 0 })
            .events
        {
            events.push(ev);
        }

        assert!(matches!(events[0], AssistantMessageEvent::Start { .. }));
        assert!(matches!(events[1], AssistantMessageEvent::TextStart { .. }));
        match &events[2] {
            AssistantMessageEvent::TextDelta { delta, partial, .. } => {
                assert_eq!(delta, "hi");
                assert_eq!(partial.content.len(), 1);
            }
            other => panic!("expected TextDelta, got {other:?}"),
        }
        match &events[3] {
            AssistantMessageEvent::TextEnd { content, .. } => assert_eq!(content, "hi"),
            other => panic!("expected TextEnd, got {other:?}"),
        }
        // Initial usage from MessageStart was captured.
        assert_eq!(state.partial.usage.input, 12);
        assert_eq!(state.partial.usage.cache_read, 4);
        assert_eq!(state.partial.usage.cache_write, 2);
    }

    #[test]
    fn streamstate_tool_call_partial_json() {
        let mut state = StreamState::new(&fake_model());
        let _ = state.process(ServerSentEvent::MessageStart {
            message: empty_a_message(),
        });
        let _ = state.process(ServerSentEvent::ContentBlockStart {
            index: 0,
            content_block: AContentBlock::ToolUseBlock {
                id: "tool_1".into(),
                input: serde_json::json!({}),
                name: "read_file".into(),
                caller: None,
            },
        });
        // Stream incomplete JSON in chunks.
        let chunks = ["{\"path\":", " \"/tmp/", "x\"}"];
        let mut last_partial: Option<Value> = None;
        for chunk in chunks {
            let outcome = state.process(ServerSentEvent::ContentBlockDelta {
                index: 0,
                delta: AContentBlockDelta::InputJsonDelta {
                    partial_json: chunk.into(),
                },
            });
            for ev in outcome.events {
                if let AssistantMessageEvent::ToolCallDelta { partial, .. } = ev
                    && let Some(AssistantContent::ToolCall(tc)) = partial.content.first()
                {
                    last_partial = Some(tc.arguments.clone());
                }
            }
        }
        // Each delta produced a usable partial; the final one should be
        // the fully parsed object.
        let final_partial = last_partial.expect("at least one delta event");
        assert_eq!(final_partial["path"], "/tmp/x");

        let outcome = state.process(ServerSentEvent::ContentBlockStop { index: 0 });
        let last = outcome.events.last().unwrap();
        match last {
            AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                assert_eq!(tool_call.id, "tool_1");
                assert_eq!(tool_call.name, "read_file");
                assert_eq!(tool_call.arguments["path"], "/tmp/x");
            }
            other => panic!("expected ToolCallEnd, got {other:?}"),
        }
    }

    #[test]
    fn streamstate_redacted_thinking_emits_thinking_events() {
        let mut state = StreamState::new(&fake_model());
        let _ = state.process(ServerSentEvent::MessageStart {
            message: empty_a_message(),
        });
        let outcome = state.process(ServerSentEvent::ContentBlockStart {
            index: 0,
            content_block: AContentBlock::RedactedThinkingBlock {
                data: "blob".into(),
            },
        });
        let stop = state.process(ServerSentEvent::ContentBlockStop { index: 0 });

        assert!(matches!(
            outcome.events[0],
            AssistantMessageEvent::ThinkingStart { .. }
        ));
        assert!(matches!(
            stop.events[0],
            AssistantMessageEvent::ThinkingEnd { .. }
        ));
        // Partial preserves the redacted flag + signature.
        match state.partial.content.first().unwrap() {
            AssistantContent::Thinking(t) => {
                assert!(t.redacted);
                assert_eq!(t.thinking_signature.as_deref(), Some("blob"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn streamstate_finalize_maps_stop_reasons() {
        for (a_stop, expected_reason) in [
            (Some(AStopReason::EndTurn), StopReason::Stop),
            (Some(AStopReason::MaxTokens), StopReason::Length),
            (Some(AStopReason::ToolUse), StopReason::ToolUse),
            (Some(AStopReason::PauseTurn), StopReason::Stop),
            (Some(AStopReason::StopSequence), StopReason::Stop),
            (None, StopReason::Stop),
        ] {
            let mut state = StreamState::new(&fake_model());
            state.stop_reason = a_stop.clone();
            let event = state.finalize();
            match event {
                AssistantMessageEvent::Done { message, .. } => {
                    assert_eq!(message.stop_reason, expected_reason, "for {:?}", a_stop);
                }
                other => panic!("expected Done for {a_stop:?}, got {other:?}"),
            }
        }

        // Refusal → Error
        let mut state = StreamState::new(&fake_model());
        state.stop_reason = Some(AStopReason::Refusal);
        state.refusal_message = Some("nope".into());
        let event = state.finalize();
        match event {
            AssistantMessageEvent::Error { error, .. } => {
                assert_eq!(error.stop_reason, StopReason::Error);
                let detail = error.error.as_ref().expect("error detail populated");
                assert_eq!(detail.category, ErrorCategory::ContentFilter);
                assert!(detail.message.contains("nope"));
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn streamstate_message_delta_updates_usage_defensively() {
        let mut state = StreamState::new(&fake_model());
        let _ = state.process(ServerSentEvent::MessageStart {
            message: empty_a_message(),
        });
        // First delta updates only output_tokens.
        let _ = state.process(ServerSentEvent::MessageDelta {
            delta: MessageDelta {
                stop_reason: None,
                stop_sequence: None,
                container: None,
                stop_details: None,
            },
            usage: AUsageDelta {
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                input_tokens: None,
                iterations: None,
                output_tokens: 7,
                server_tool_use: None,
            },
            context_management: None,
        });
        // input_tokens/cache_read/cache_write preserved from MessageStart.
        assert_eq!(state.partial.usage.input, 12);
        assert_eq!(state.partial.usage.cache_read, 4);
        assert_eq!(state.partial.usage.cache_write, 2);
        assert_eq!(state.partial.usage.output, 7);

        // Second delta refreshes input_tokens.
        let _ = state.process(ServerSentEvent::MessageDelta {
            delta: MessageDelta {
                stop_reason: Some(AStopReason::EndTurn),
                stop_sequence: None,
                container: None,
                stop_details: None,
            },
            usage: AUsageDelta {
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                input_tokens: Some(20),
                iterations: None,
                output_tokens: 9,
                server_tool_use: None,
            },
            context_management: None,
        });
        assert_eq!(state.partial.usage.input, 20);
        assert_eq!(state.partial.usage.output, 9);
        assert!(matches!(state.stop_reason, Some(AStopReason::EndTurn)));
    }

    #[test]
    fn streamstate_finalize_computes_total_and_cost() {
        let mut state = StreamState::new(&fake_model());
        state.partial.usage.input = 1_000;
        state.partial.usage.output = 500;
        state.partial.usage.cache_read = 100;
        state.partial.usage.cache_write = 50;
        state.stop_reason = Some(AStopReason::EndTurn);
        let event = state.finalize();
        match event {
            AssistantMessageEvent::Done { message, .. } => {
                assert_eq!(message.usage.total_tokens, 1_650);
                // 1000 * 3.0/1e6 + 500 * 15.0/1e6 + 100 * 0.3/1e6 + 50 * 3.75/1e6
                let expected = 0.003 + 0.0075 + 0.000_03 + 0.000_187_5;
                assert!(
                    (message.usage.cost.total - expected).abs() < 1e-9,
                    "got {} expected {}",
                    message.usage.cost.total,
                    expected
                );
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn extra_betas_from_headers_returns_empty_when_no_headers() {
        assert!(extra_betas_from_headers(None).is_empty());
    }

    #[test]
    fn extra_betas_from_headers_skips_unrelated_keys() {
        use std::collections::HashMap;
        let mut headers = HashMap::new();
        headers.insert("x-custom-header".into(), "value".into());
        headers.insert("authorization".into(), "Bearer x".into());
        assert!(extra_betas_from_headers(Some(&headers)).is_empty());
    }

    #[test]
    fn extra_betas_from_headers_extracts_single_beta() {
        use std::collections::HashMap;
        let mut headers = HashMap::new();
        headers.insert("anthropic-beta".into(), "fast-mode-2026-02-01".into());
        let betas = extra_betas_from_headers(Some(&headers));
        assert_eq!(betas, vec!["fast-mode-2026-02-01".to_string()]);
    }

    #[test]
    fn extra_betas_from_headers_splits_comma_separated_values() {
        use std::collections::HashMap;
        let mut headers = HashMap::new();
        headers.insert("anthropic-beta".into(), "alpha-1, beta-2 ,gamma-3".into());
        let betas = extra_betas_from_headers(Some(&headers));
        assert_eq!(
            betas,
            vec![
                "alpha-1".to_string(),
                "beta-2".to_string(),
                "gamma-3".to_string(),
            ]
        );
    }

    #[test]
    fn extra_betas_from_headers_matches_case_insensitively() {
        use std::collections::HashMap;
        let mut headers = HashMap::new();
        headers.insert("Anthropic-Beta".into(), "mixed-case-1".into());
        let betas = extra_betas_from_headers(Some(&headers));
        assert_eq!(betas, vec!["mixed-case-1".to_string()]);
    }

    #[test]
    fn extra_betas_from_headers_drops_empty_entries() {
        use std::collections::HashMap;
        let mut headers = HashMap::new();
        headers.insert("anthropic-beta".into(), ",a,,b,".into());
        let betas = extra_betas_from_headers(Some(&headers));
        assert_eq!(betas, vec!["a".to_string(), "b".to_string()]);
    }
}
