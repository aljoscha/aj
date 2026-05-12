//! OpenAI Responses API provider.
//!
//! Implements the unified [`Provider`] trait against OpenAI's
//! `POST /responses` streaming endpoint. See `docs/models-spec.md` §7.3.
//!
//! Stateless — per-call HTTP knobs (auth, base URL, reasoning effort,
//! tool choice, session correlation) are derived from the per-call
//! [`ModelInfo`] and [`StreamOptions`] so the same instance can serve
//! any number of concurrent requests.
//!
//! Unlike the Chat Completions provider in [`super::provider`], this
//! API preserves encrypted reasoning across turns: prior-turn
//! [`ThinkingContent`] blocks are carried through `thinking_signature`
//! and replayed back into the `input` array as `reasoning` items, and
//! per-message `id` / `phase` are round-tripped via the
//! [`TextSignatureV1`] envelope on `text_signature`.

use std::collections::HashMap;

use futures::StreamExt;
use openai_sdk::client::{Client, ClientError};
use openai_sdk::types::common::{
    PromptCacheRetention, ReasoningEffort, ServiceTier as OpenAIServiceTier,
};
use openai_sdk::types::responses::{
    CreateResponseRequest, FunctionCallOutputContent, ImageDetail, InputRole, ItemStatus,
    MessagePhase, Reasoning, ReasoningSummary, ReasoningSummaryMode, Response, ResponseIncludable,
    ResponseInput, ResponseInputContentPart, ResponseInputItem, ResponseInputMessageContent,
    ResponseOutputItem, ResponseStatus, ResponseStreamEvent, ResponseTool, ResponseToolChoice,
    ResponseUsage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::{classify_openai_error, parse_retry_after, transport_error};
use crate::partial_json::parse_streaming_json;
use crate::provider::Provider;
use crate::registry::{ModelInfo, calculate_cost, supports_xhigh};
use crate::streaming::{
    AssistantMessageEvent, AssistantMessageEventStream, DoneReason, ErrorReason,
};
use crate::transform::transform_messages;
use crate::types::{
    AssistantContent, AssistantError, AssistantMessage, CacheRetention, Context, ErrorCategory,
    Message, ReasoningSummary as UnifiedReasoningSummary, ServiceTier, SimpleStreamOptions,
    StopReason, StreamOptions, TextContent, ThinkingContent, ThinkingLevel, ToolCall, ToolChoice,
    ToolDefinition, ToolResultMessage, UserContent, UserMessage,
};

/// `api` field reported on assistant messages produced by this provider.
const API_NAME: &str = "openai-responses";
/// Hard limit on item / message IDs accepted by the Responses API.
pub(super) const ID_LIMIT: usize = 64;

/// Stateless provider for the OpenAI Responses API.
pub struct OpenAiResponsesProvider;

impl Provider for OpenAiResponsesProvider {
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
// TextSignatureV1 (§7.3.4)
// ---------------------------------------------------------------------------

/// Envelope carried in [`TextContent::text_signature`] for messages
/// produced by `openai-responses`. Captures the message item's `id`
/// and optional `phase` so a follow-up turn can replay the message
/// with the same identifiers, letting the server pair it with the
/// prior reasoning chain.
#[derive(Debug, Serialize, Deserialize)]
pub struct TextSignatureV1 {
    /// Schema version. Always `1`.
    pub v: u8,
    /// Message item id (e.g. `"msg_abc123"`).
    pub id: String,
    /// `"commentary"` or `"final_answer"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<MessagePhase>,
}

#[derive(Debug, Default)]
pub(super) struct ParsedTextSignature {
    pub(super) id: Option<String>,
    pub(super) phase: Option<MessagePhase>,
}

pub(super) fn parse_text_signature(signature: Option<&str>) -> ParsedTextSignature {
    let Some(signature) = signature else {
        return ParsedTextSignature::default();
    };
    if let Ok(parsed) = serde_json::from_str::<TextSignatureV1>(signature) {
        return ParsedTextSignature {
            id: Some(parsed.id),
            phase: parsed.phase,
        };
    }
    // Legacy plain-id format: treat the whole string as the id.
    ParsedTextSignature {
        id: Some(signature.to_string()),
        phase: None,
    }
}

pub(super) fn serialize_text_signature(id: String, phase: Option<MessagePhase>) -> Option<String> {
    let env = TextSignatureV1 { v: 1, id, phase };
    serde_json::to_string(&env).ok()
}

pub(super) fn normalize_replay_message_id(id: String) -> String {
    if id.len() <= ID_LIMIT {
        id
    } else {
        format!("msg_{}", short_hash(&id))
    }
}

/// Stable 12-hex FNV-1a digest. Used to rewrite over-long IDs.
fn short_hash(s: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h: u64 = FNV_OFFSET;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(FNV_PRIME);
    }
    let hex = format!("{h:016x}");
    hex[..12].to_string()
}

// ---------------------------------------------------------------------------
// Composite tool-call IDs (§7.3.5)
// ---------------------------------------------------------------------------

pub(super) fn split_tool_use_id(tool_use_id: &str) -> (String, Option<String>) {
    if let Some((call_id, item_id)) = tool_use_id.split_once('|') {
        (call_id.to_string(), Some(item_id.to_string()))
    } else {
        (tool_use_id.to_string(), None)
    }
}

pub(super) fn compose_tool_use_id(call_id: &str, item_id: Option<&str>) -> String {
    match item_id {
        Some(item_id) if !item_id.is_empty() => format!("{call_id}|{item_id}"),
        _ => call_id.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Stream entry point
// ---------------------------------------------------------------------------

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
        producer.end();
    });
    stream
}

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

async fn run_stream_inner(
    producer: &AssistantMessageEventStream,
    model: &ModelInfo,
    context: &Context,
    options: &StreamOptions,
    reasoning: Option<&ThinkingLevel>,
) -> Result<(), AssistantError> {
    let api_key = options.api_key.clone().ok_or_else(|| {
        AssistantError::new(
            ErrorCategory::Auth,
            "openai-responses provider requires StreamOptions.api_key",
        )
    })?;

    let base_url_present = !model.base_url.is_empty();
    let base_url_opt = base_url_present.then(|| model.base_url.clone());
    let base_url_for_check = base_url_opt
        .clone()
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let mut client = Client::new(base_url_opt, api_key);

    // §7.3 preface: forward session_id as session-correlation headers
    // when the request is going to api.openai.com. Other deployments
    // (Azure, etc.) may reject unknown headers, so guard on hostname.
    if is_openai_host(&base_url_for_check) {
        if let Some(sid) = options.session_id.as_deref() {
            client = client
                .with_extra_header("session_id", sid)
                .with_extra_header("x-client-request-id", sid);
        }
    }

    let request = build_request(model, context, options, reasoning);

    if let Some(cb) = options.on_payload.as_ref() {
        match serde_json::to_value(&request) {
            Ok(body) => cb.call(&body),
            Err(err) => tracing::warn!("on_payload serialization failed: {err}"),
        }
    }

    let mut sse = client
        .responses_stream(request)
        .await
        .map_err(|err| classify_client_error(&err))?;

    let mut state = StreamState::new(model, options.service_tier.clone());

    while let Some(event) = sse.next().await {
        match event {
            Ok(ev) => {
                for out in state.process(ev) {
                    producer.push(out);
                }
            }
            Err(err) => return Err(classify_client_error(&err)),
        }
    }

    let final_event = state.finalize();
    producer.push(final_event);
    Ok(())
}

pub(super) fn classify_client_error(err: &ClientError) -> AssistantError {
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

pub(super) fn is_openai_host(base_url: &str) -> bool {
    // Match on the canonical host to avoid sending session-correlation
    // headers to Azure/proxy deployments that may reject them.
    base_url.contains("//api.openai.com")
}

// ---------------------------------------------------------------------------
// Request body construction (§7.3.2)
// ---------------------------------------------------------------------------

fn build_request(
    model: &ModelInfo,
    context: &Context,
    options: &StreamOptions,
    reasoning: Option<&ThinkingLevel>,
) -> CreateResponseRequest {
    let mut input: Vec<ResponseInputItem> = Vec::new();
    if let Some(prompt) = context.system_prompt.as_deref()
        && !prompt.is_empty()
    {
        input.push(build_system_item(model, prompt));
    }

    // §8: cross-provider history rewrite first.
    let transformed = transform_messages(&context.messages, model);
    convert_messages(API_NAME, &transformed, &mut input);

    let tools: Vec<ResponseTool> = context.tools.iter().map(to_response_tool).collect();
    let tool_choice = to_response_tool_choice(options.tool_choice.as_ref(), !tools.is_empty());

    let max_output_tokens = options
        .max_tokens
        .map(|t| u32::try_from(t).unwrap_or(u32::MAX));

    // §7.3.2 reasoning configuration. Non-reasoning models reject the
    // `reasoning` parameter entirely.
    let (reasoning_cfg, include) = if model.reasoning {
        match reasoning {
            Some(level) => {
                let summary = match options.reasoning_summary.as_ref() {
                    Some(UnifiedReasoningSummary::Auto) | None => ReasoningSummaryMode::Auto,
                    Some(UnifiedReasoningSummary::Detailed) => ReasoningSummaryMode::Detailed,
                    Some(UnifiedReasoningSummary::Concise) => ReasoningSummaryMode::Concise,
                };
                (
                    Some(Reasoning {
                        effort: Some(map_reasoning_effort(Some(level), model)),
                        summary: Some(summary),
                    }),
                    vec![ResponseIncludable::ReasoningEncryptedContent],
                )
            }
            None => (
                Some(Reasoning {
                    effort: Some(ReasoningEffort::None),
                    summary: None,
                }),
                Vec::new(),
            ),
        }
    } else {
        (None, Vec::new())
    };

    // §7.3.2 prompt caching: Responses caching is automatic; these
    // fields are routing/retention hints.
    let prompt_cache_key = match (
        options.cache_retention.clone(),
        options.session_id.as_deref(),
    ) {
        (CacheRetention::None, _) | (_, None) => None,
        (_, Some(sid)) => Some(sid.to_string()),
    };
    let prompt_cache_retention = match (
        options.cache_retention.clone(),
        is_openai_host(&model.base_url),
    ) {
        (CacheRetention::Long, true) => Some(PromptCacheRetention::TwentyFourHours),
        _ => None,
    };

    let service_tier = options.service_tier.as_ref().map(map_service_tier);

    CreateResponseRequest {
        model: model.id.clone(),
        input: ResponseInput::Items(input),
        tools,
        tool_choice,
        parallel_tool_calls: Some(true),
        max_output_tokens,
        temperature: options.temperature,
        reasoning: reasoning_cfg,
        stream: Some(true),
        store: Some(false),
        include,
        service_tier,
        prompt_cache_key,
        prompt_cache_retention,
        ..Default::default()
    }
}

fn build_system_item(model: &ModelInfo, prompt: &str) -> ResponseInputItem {
    if model.reasoning {
        ResponseInputItem::developer_text(prompt.to_string())
    } else {
        ResponseInputItem::system_text(prompt.to_string())
    }
}

pub(super) fn map_reasoning_effort(
    level: Option<&ThinkingLevel>,
    model: &ModelInfo,
) -> ReasoningEffort {
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
                // §7.3.2: XHigh is GPT-5.2+ only; fall back to High.
                ReasoningEffort::High
            }
        }
    }
}

pub(super) fn map_service_tier(tier: &ServiceTier) -> OpenAIServiceTier {
    match tier {
        ServiceTier::Flex => OpenAIServiceTier::Flex,
        ServiceTier::Priority => OpenAIServiceTier::Priority,
    }
}

pub(super) fn responses_cost_multiplier(
    _model_id: &str,
    server_tier: Option<&OpenAIServiceTier>,
    requested_tier: Option<&OpenAIServiceTier>,
) -> f64 {
    cost_multiplier_from_tier(server_tier.or(requested_tier))
}

fn cost_multiplier_from_tier(tier: Option<&OpenAIServiceTier>) -> f64 {
    match tier {
        Some(OpenAIServiceTier::Flex) => 0.5,
        Some(OpenAIServiceTier::Priority) => 2.0,
        _ => 1.0,
    }
}

// ---------------------------------------------------------------------------
// Tools (§7.3.2)
// ---------------------------------------------------------------------------

fn to_response_tool(tool: &ToolDefinition) -> ResponseTool {
    ResponseTool::Function {
        name: tool.name.clone(),
        description: Some(tool.description.clone()),
        parameters: Some(tool.parameters.clone()),
        // §7.3.2 hardcodes `strict: false`.
        strict: Some(false),
    }
}

fn to_response_tool_choice(
    choice: Option<&ToolChoice>,
    has_tools: bool,
) -> Option<ResponseToolChoice> {
    match choice {
        None => None,
        _ if !has_tools => None,
        Some(ToolChoice::Auto) => Some(ResponseToolChoice::String("auto".to_string())),
        Some(ToolChoice::None) => Some(ResponseToolChoice::String("none".to_string())),
        Some(ToolChoice::Required) => Some(ResponseToolChoice::String("required".to_string())),
        Some(ToolChoice::Tool { name }) => Some(ResponseToolChoice::Function {
            r#type: "function".to_string(),
            name: name.clone(),
        }),
    }
}

// ---------------------------------------------------------------------------
// Message conversion (§7.3.1)
// ---------------------------------------------------------------------------

/// Project the unified message log onto Responses input items.
///
/// `api_name` controls the cross-model check inside
/// [`append_assistant_message`]: an assistant message tagged with an
/// `api` that differs from the current provider's identifier is
/// treated as cross-model replay (per §7.3.1) and its tool-call
/// `item_id`s are dropped so the server doesn't try to pair them
/// with reasoning items it never produced.
pub(super) fn convert_messages(
    api_name: &str,
    messages: &[Message],
    out: &mut Vec<ResponseInputItem>,
) {
    for msg in messages {
        match msg {
            Message::User(u) => append_user_message(u, out),
            Message::Assistant(a) => append_assistant_message(api_name, a, out),
            Message::ToolResult(tr) => out.push(convert_tool_result(tr)),
        }
    }
}

fn append_user_message(m: &UserMessage, out: &mut Vec<ResponseInputItem>) {
    let parts: Vec<ResponseInputContentPart> =
        m.content.iter().map(user_content_to_input_part).collect();
    if parts.is_empty() {
        return;
    }
    out.push(ResponseInputItem::Message {
        id: None,
        role: InputRole::User,
        content: ResponseInputMessageContent::Array(parts),
        status: None,
        phase: None,
    });
}

pub(super) fn user_content_to_input_part(c: &UserContent) -> ResponseInputContentPart {
    match c {
        UserContent::Text(t) => ResponseInputContentPart::InputText {
            text: t.text.clone(),
        },
        UserContent::Image(img) => ResponseInputContentPart::InputImage {
            image_url: Some(format!("data:{};base64,{}", img.mime_type, img.data)),
            file_id: None,
            detail: Some(ImageDetail::Auto),
        },
    }
}

/// Expand an assistant message into typed Responses input items, in
/// `AssistantContent` order. Reasoning items deserialize from
/// `thinking_signature`; text blocks reuse / split message items by
/// (id, phase); tool calls split the composite `{call_id}|{item_id}`.
pub(super) fn append_assistant_message(
    api_name: &str,
    m: &AssistantMessage,
    out: &mut Vec<ResponseInputItem>,
) {
    let cross_model = !m.api.is_empty() && m.api != api_name;

    let mut pending_parts: Vec<ResponseInputContentPart> = Vec::new();
    let mut pending_id: Option<String> = None;
    let mut pending_phase: Option<MessagePhase> = None;

    for block in &m.content {
        match block {
            AssistantContent::Text(t) => {
                let sig = parse_text_signature(t.text_signature.as_deref());
                let next_id = sig.id.map(normalize_replay_message_id);
                let next_phase = sig.phase;

                // Group consecutive text parts into one Message item
                // when they share the same (id, phase). On any drift,
                // flush before opening a new group.
                if !pending_parts.is_empty()
                    && (pending_id != next_id || pending_phase != next_phase)
                {
                    flush_assistant_message(
                        out,
                        &mut pending_parts,
                        &mut pending_id,
                        &mut pending_phase,
                    );
                }
                if pending_parts.is_empty() {
                    pending_id = next_id;
                    pending_phase = next_phase;
                }
                pending_parts.push(ResponseInputContentPart::OutputText {
                    text: t.text.clone(),
                    annotations: Vec::new(),
                    logprobs: None,
                });
            }
            AssistantContent::Thinking(th) => {
                flush_assistant_message(
                    out,
                    &mut pending_parts,
                    &mut pending_id,
                    &mut pending_phase,
                );
                if let Some(sig) = th.thinking_signature.as_deref() {
                    if let Some(item) = reasoning_item_from_signature(sig) {
                        out.push(item);
                    }
                    // Signatures that don't deserialize into a
                    // `ResponseInputItem::Reasoning` (e.g. cross-
                    // provider stale strings) are dropped silently —
                    // the visible text was already demoted to plain
                    // text by §8.1 rule 2 before reaching here.
                }
                // Thinking with no signature is dropped: §7.3.1 says
                // unsigned thinking demotes to plain text upstream;
                // any that survives here has been intentionally kept
                // by the same-model branch and has nothing to replay.
            }
            AssistantContent::ToolCall(tc) => {
                flush_assistant_message(
                    out,
                    &mut pending_parts,
                    &mut pending_id,
                    &mut pending_phase,
                );
                let (call_id, item_id) = split_tool_use_id(&tc.id);
                // §7.3.1 cross-model replay: omit `id` when the call
                // came from a different api/model so the server does
                // not try to pair it with reasoning items it never
                // emitted.
                let item_id = if cross_model { None } else { item_id };
                out.push(ResponseInputItem::FunctionCall {
                    id: item_id,
                    call_id,
                    name: tc.name.clone(),
                    arguments: tc.arguments.to_string(),
                    status: Some(ItemStatus::Completed),
                });
            }
        }
    }
    flush_assistant_message(out, &mut pending_parts, &mut pending_id, &mut pending_phase);
}

fn flush_assistant_message(
    out: &mut Vec<ResponseInputItem>,
    parts: &mut Vec<ResponseInputContentPart>,
    id: &mut Option<String>,
    phase: &mut Option<MessagePhase>,
) {
    if parts.is_empty() {
        return;
    }
    out.push(ResponseInputItem::Message {
        id: id.take(),
        role: InputRole::Assistant,
        content: ResponseInputMessageContent::Array(std::mem::take(parts)),
        status: Some(ItemStatus::Completed),
        phase: phase.take(),
    });
}

fn reasoning_item_from_signature(signature: &str) -> Option<ResponseInputItem> {
    match serde_json::from_str::<ResponseInputItem>(signature) {
        Ok(item @ ResponseInputItem::Reasoning { .. }) => Some(item),
        _ => None,
    }
}

pub(super) fn convert_tool_result(t: &ToolResultMessage) -> ResponseInputItem {
    let (call_id, _) = split_tool_use_id(&t.tool_call_id);

    // Split content into text + image parts; the Responses API supports
    // an array form for `output` so we can interleave images inline.
    let mut text_buf = String::new();
    let mut image_parts: Vec<ResponseInputContentPart> = Vec::new();
    for c in &t.content {
        match c {
            UserContent::Text(text) => text_buf.push_str(&text.text),
            UserContent::Image(_) => image_parts.push(user_content_to_input_part(c)),
        }
    }

    let output = if image_parts.is_empty() {
        if text_buf.is_empty() {
            // Same fallback as §7.2: keep the model from seeing an empty
            // tool result, which it can't react to usefully.
            FunctionCallOutputContent::String(if t.is_error {
                "[tool returned an error with no text payload]".to_string()
            } else {
                "[tool returned no text]".to_string()
            })
        } else {
            FunctionCallOutputContent::String(text_buf)
        }
    } else {
        let mut parts = Vec::with_capacity(image_parts.len() + 1);
        if !text_buf.is_empty() {
            parts.push(ResponseInputContentPart::InputText { text: text_buf });
        }
        parts.extend(image_parts);
        FunctionCallOutputContent::Array(parts)
    };

    ResponseInputItem::FunctionCallOutput {
        call_id,
        output,
        id: None,
        status: None,
    }
}

// ---------------------------------------------------------------------------
// Public round-trip helpers (§1.10, §12 step 11b)
// ---------------------------------------------------------------------------

/// Serialize side of the §1.10 invariant for `openai-responses`: project
/// an [`AssistantMessage`] onto the typed input items the Responses API
/// expects on the request side.
///
/// One assistant message expands to multiple input items in
/// `AssistantContent` order — reasoning items, then message items
/// grouped by `(id, phase)`, interleaved with `function_call` items.
/// Cross-model replay rules (§7.3.1) are honoured: `id` on
/// `function_call` items is omitted when the assistant message came
/// from a different api so the server doesn't try to pair it with
/// reasoning items it never produced.
pub fn assistant_message_to_input_items(message: &AssistantMessage) -> Vec<ResponseInputItem> {
    let mut out = Vec::new();
    append_assistant_message(API_NAME, message, &mut out);
    out
}

/// Inverse of [`assistant_message_to_input_items`]: parse a sequence of
/// Responses `input` items whose role is `assistant` (plus interleaved
/// reasoning / function_call items) back into a unified
/// [`AssistantMessage`].
///
/// Symmetric to the streaming state machine, exposed publicly so the
/// round-trip suite can replay request bodies through the same parse
/// path.
pub fn parse_assistant_input_items(items: &[ResponseInputItem]) -> AssistantMessage {
    parse_assistant_input_items_with_api(API_NAME, items)
}

/// Like [`parse_assistant_input_items`] but lets the caller pin the
/// `api` field on the returned message. Used by sibling providers
/// (`openai-codex-responses`) that share the Responses wire shape but
/// have their own api identifier.
pub(super) fn parse_assistant_input_items_with_api(
    api_name: &str,
    items: &[ResponseInputItem],
) -> AssistantMessage {
    let mut out = AssistantMessage::empty();
    out.api = api_name.to_string();
    for item in items {
        match item {
            ResponseInputItem::Reasoning { .. } => {
                let signature = serde_json::to_string(item).ok();
                let summary = match item {
                    ResponseInputItem::Reasoning { summary, .. } => join_reasoning_summary(summary),
                    _ => unreachable!(),
                };
                out.content
                    .push(AssistantContent::Thinking(ThinkingContent {
                        thinking: summary,
                        thinking_signature: signature,
                        redacted: false,
                    }));
            }
            ResponseInputItem::Message {
                role,
                content,
                id,
                phase,
                ..
            } => {
                if !matches!(role, InputRole::Assistant) {
                    continue;
                }
                let signature = id
                    .as_ref()
                    .and_then(|id| serialize_text_signature(id.clone(), phase.clone()));
                push_message_text(&mut out, content, signature.as_deref());
            }
            ResponseInputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
                ..
            } => {
                let arguments_json: Value = if arguments.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(arguments)
                        .unwrap_or_else(|_| parse_streaming_json(arguments))
                };
                out.content.push(AssistantContent::ToolCall(ToolCall {
                    id: compose_tool_use_id(call_id, id.as_deref()),
                    name: name.clone(),
                    arguments: arguments_json,
                }));
            }
            ResponseInputItem::FunctionCallOutput { .. }
            | ResponseInputItem::ItemReference { .. } => {
                // Tool results / references are not assistant content;
                // they live as their own `Message` variants.
            }
        }
    }
    out
}

fn push_message_text(
    out: &mut AssistantMessage,
    content: &ResponseInputMessageContent,
    signature: Option<&str>,
) {
    match content {
        ResponseInputMessageContent::String(s) => {
            if !s.is_empty() {
                out.content.push(AssistantContent::Text(TextContent {
                    text: s.clone(),
                    text_signature: signature.map(str::to_string),
                }));
            }
        }
        ResponseInputMessageContent::Array(parts) => {
            for part in parts {
                if let ResponseInputContentPart::OutputText { text, .. }
                | ResponseInputContentPart::Refusal { refusal: text } = part
                {
                    if !text.is_empty() {
                        out.content.push(AssistantContent::Text(TextContent {
                            text: text.clone(),
                            text_signature: signature.map(str::to_string),
                        }));
                    }
                }
            }
        }
    }
}

/// Replay a sequence of pre-decoded Responses stream events through
/// the provider's state machine and return the finalized
/// [`AssistantMessage`]. Mirror of
/// [`crate::openai::provider::replay_sse_events`].
pub fn replay_sse_events(
    model: &ModelInfo,
    events: impl IntoIterator<Item = ResponseStreamEvent>,
    requested_tier: Option<ServiceTier>,
) -> AssistantMessage {
    let mut state = StreamState::new(model, requested_tier);
    for ev in events {
        let _ = state.process(ev);
    }
    match state.finalize() {
        AssistantMessageEvent::Done { message, .. }
        | AssistantMessageEvent::Error { error: message, .. } => message,
        other => panic!("StreamState::finalize returned non-terminal event: {other:?}"),
    }
}

fn join_reasoning_summary(summary: &[ReasoningSummary]) -> String {
    summary
        .iter()
        .map(|p| p.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

// ---------------------------------------------------------------------------
// Stream state machine (§7.3.6)
// ---------------------------------------------------------------------------

/// Cost-multiplier strategy. Codex uses a different curve than the
/// public Responses API, so providers inject their own multiplier when
/// constructing a [`StreamState`].
///
/// Arguments:
/// - `model_id` — the model the assistant message ran against (the
///   `gpt-5.5` exception in §7.4.4 keys off this).
/// - `server_tier` — `response.service_tier` echoed back by the server.
/// - `requested_tier` — the tier requested by the caller (used as a
///   fallback when the server doesn't echo, and as the "intended" tier
///   when the server echoes `default` despite the request).
pub(super) type CostMultiplierFn = fn(
    model_id: &str,
    server_tier: Option<&OpenAIServiceTier>,
    requested_tier: Option<&OpenAIServiceTier>,
) -> f64;

#[derive(Debug)]
#[allow(dead_code)]
enum ItemSlot {
    /// Reasoning output item: a single Thinking block in
    /// `partial.content`. Tracks how many summary parts we've seen so
    /// part separators only emit on the second-and-later parts.
    Reasoning {
        content_index: usize,
        item_id: String,
        seen_part_count: u32,
    },
    /// Assistant message output item. Each `(content_index)` part is
    /// projected as a separate Text block; the Message item's id /
    /// phase are baked into a `text_signature` on every block at
    /// `output_item.done`.
    Message {
        item_id: String,
        text_blocks: HashMap<u32, usize>,
    },
    /// Function-call output item. Accumulates arguments bytes until
    /// either `function_call_arguments.done` or `output_item.done`.
    FunctionCall {
        content_index: usize,
        call_id: String,
        item_id: Option<String>,
        arguments: String,
    },
}

pub(super) struct StreamState {
    partial: AssistantMessage,
    started: bool,
    /// Slots keyed by `output_index` — stable per output item.
    slots: HashMap<u32, ItemSlot>,
    /// Captured terminal Response (from `response.completed` /
    /// `response.incomplete`).
    final_response: Option<Response>,
    /// Status seen on a terminal lifecycle event.
    finish_status: Option<ResponseStatus>,
    /// Error pulled out of `response.failed` / SSE `error`.
    finish_error: Option<AssistantError>,
    /// Tier requested by the caller; preserved for cost calculations
    /// when the server doesn't echo it back.
    requested_tier: Option<OpenAIServiceTier>,
    /// Provider api identifier stamped on terminal error messages
    /// (`api_name: <reason>`).
    api_name: &'static str,
    /// Cost multiplier strategy for this provider; see [`CostMultiplierFn`].
    cost_multiplier: CostMultiplierFn,
}

impl StreamState {
    pub(super) fn new(model: &ModelInfo, requested_tier: Option<ServiceTier>) -> Self {
        const RESPONSES_COST_MULTIPLIER: CostMultiplierFn = responses_cost_multiplier;
        Self::new_with(API_NAME, model, requested_tier, RESPONSES_COST_MULTIPLIER)
    }

    /// Provider-customizable constructor used by Codex (see
    /// `openai::codex`) to pick its own api name and cost-multiplier
    /// curve while reusing the §7.3 streaming machinery.
    pub(super) fn new_with(
        api_name: &'static str,
        model: &ModelInfo,
        requested_tier: Option<ServiceTier>,
        cost_multiplier: CostMultiplierFn,
    ) -> Self {
        let mut partial = AssistantMessage::empty();
        partial.api = api_name.to_string();
        partial.provider = model.provider.clone();
        partial.model = model.id.clone();
        Self {
            partial,
            started: false,
            slots: HashMap::new(),
            final_response: None,
            finish_status: None,
            finish_error: None,
            requested_tier: requested_tier.as_ref().map(map_service_tier),
            api_name,
            cost_multiplier,
        }
    }

    pub(super) fn process(&mut self, event: ResponseStreamEvent) -> Vec<AssistantMessageEvent> {
        let mut out = Vec::new();
        match event {
            ResponseStreamEvent::ResponseCreated { response, .. }
            | ResponseStreamEvent::ResponseInProgress { response, .. }
            | ResponseStreamEvent::ResponseQueued { response, .. } => {
                self.ensure_started(&response, &mut out);
            }
            ResponseStreamEvent::OutputItemAdded {
                item, output_index, ..
            } => self.on_output_item_added(item, output_index, &mut out),
            ResponseStreamEvent::OutputItemDone {
                item, output_index, ..
            } => self.on_output_item_done(item, output_index, &mut out),
            ResponseStreamEvent::ContentPartAdded { .. }
            | ResponseStreamEvent::ContentPartDone { .. }
            | ResponseStreamEvent::OutputTextAnnotationAdded { .. } => {}
            ResponseStreamEvent::OutputTextDelta {
                delta,
                output_index,
                content_index,
                ..
            }
            | ResponseStreamEvent::RefusalDelta {
                delta,
                output_index,
                content_index,
                ..
            } => self.on_text_delta(output_index, content_index, delta, &mut out),
            ResponseStreamEvent::OutputTextDone { .. }
            | ResponseStreamEvent::RefusalDone { .. } => {
                // The accumulated snapshot in partial.content already
                // matches the final text; rely on output_item.done to
                // close the block.
            }
            ResponseStreamEvent::FunctionCallArgumentsDelta {
                delta,
                output_index,
                ..
            } => self.on_function_args_delta(output_index, &delta, &mut out),
            ResponseStreamEvent::FunctionCallArgumentsDone { .. } => {
                // The streaming arguments buffer is replaced with the
                // canonical `arguments` string on output_item.done; no
                // separate event needed here.
            }
            ResponseStreamEvent::ReasoningSummaryPartAdded { output_index, .. } => {
                self.on_reasoning_summary_part_added(output_index, &mut out)
            }
            ResponseStreamEvent::ReasoningSummaryTextDelta {
                delta,
                output_index,
                ..
            } => self.on_reasoning_delta(output_index, &delta, &mut out),
            ResponseStreamEvent::ReasoningSummaryPartDone { .. }
            | ResponseStreamEvent::ReasoningSummaryTextDone { .. }
            | ResponseStreamEvent::ReasoningTextDelta { .. }
            | ResponseStreamEvent::ReasoningTextDone { .. } => {}
            ResponseStreamEvent::ResponseCompleted { response, .. } => {
                self.ensure_started(&response, &mut out);
                self.finish_status = response.status.clone();
                self.final_response = Some(response);
            }
            ResponseStreamEvent::ResponseIncomplete { response, .. } => {
                self.ensure_started(&response, &mut out);
                self.finish_status = response.status.clone().or(Some(ResponseStatus::Incomplete));
                self.final_response = Some(response);
            }
            ResponseStreamEvent::ResponseFailed { response, .. } => {
                self.ensure_started(&response, &mut out);
                self.finish_error = Some(error_from_response(&response));
                self.finish_status = response.status.clone().or(Some(ResponseStatus::Failed));
                self.final_response = Some(response);
            }
            ResponseStreamEvent::Error { code, message, .. } => {
                self.finish_error = Some(error_from_code(code.as_deref(), message));
                self.finish_status = Some(ResponseStatus::Failed);
            }
            ResponseStreamEvent::WebSearchCallInProgress { .. }
            | ResponseStreamEvent::WebSearchCallSearching { .. }
            | ResponseStreamEvent::WebSearchCallCompleted { .. }
            | ResponseStreamEvent::Other(_) => {}
        }
        out
    }

    fn ensure_started(&mut self, response: &Response, out: &mut Vec<AssistantMessageEvent>) {
        if self.started {
            return;
        }
        self.started = true;
        self.partial.response_id = Some(response.id.clone());
        if !response.model.is_empty() {
            self.partial.model = response.model.clone();
        }
        out.push(AssistantMessageEvent::Start {
            partial: self.partial.clone(),
        });
    }

    fn on_output_item_added(
        &mut self,
        item: ResponseOutputItem,
        output_index: u32,
        out: &mut Vec<AssistantMessageEvent>,
    ) {
        match item {
            ResponseOutputItem::Reasoning { id, .. } => {
                let content_index = self.partial.content.len();
                self.partial
                    .content
                    .push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: false,
                    }));
                self.slots.insert(
                    output_index,
                    ItemSlot::Reasoning {
                        content_index,
                        item_id: id,
                        seen_part_count: 0,
                    },
                );
                out.push(AssistantMessageEvent::ThinkingStart {
                    content_index,
                    partial: self.partial.clone(),
                });
            }
            ResponseOutputItem::Message { id, .. } => {
                self.slots.insert(
                    output_index,
                    ItemSlot::Message {
                        item_id: id,
                        text_blocks: HashMap::new(),
                    },
                );
                // TextStart deferred until first delta arrives — a
                // message item with no parts produces no Text block.
            }
            ResponseOutputItem::FunctionCall {
                id,
                call_id,
                name,
                arguments,
                ..
            } => {
                let content_index = self.partial.content.len();
                let composite = compose_tool_use_id(&call_id, id.as_deref());
                self.partial
                    .content
                    .push(AssistantContent::ToolCall(ToolCall {
                        id: composite,
                        name,
                        arguments: Value::Object(serde_json::Map::new()),
                    }));
                self.slots.insert(
                    output_index,
                    ItemSlot::FunctionCall {
                        content_index,
                        call_id,
                        item_id: id,
                        arguments,
                    },
                );
                out.push(AssistantMessageEvent::ToolCallStart {
                    content_index,
                    partial: self.partial.clone(),
                });
            }
            ResponseOutputItem::WebSearchCall { .. } | ResponseOutputItem::Other(_) => {}
        }
    }

    fn on_output_item_done(
        &mut self,
        item: ResponseOutputItem,
        output_index: u32,
        out: &mut Vec<AssistantMessageEvent>,
    ) {
        let slot = self.slots.remove(&output_index);
        match (item, slot) {
            (
                ResponseOutputItem::Reasoning {
                    id,
                    summary,
                    content,
                    encrypted_content,
                    status,
                },
                Some(ItemSlot::Reasoning { content_index, .. }),
            ) => {
                // Re-serialize the reasoning item into a stable
                // signature so the next turn can replay it.
                let signature = serde_json::to_string(&ResponseInputItem::Reasoning {
                    id,
                    summary: summary.clone(),
                    content,
                    encrypted_content,
                    status,
                })
                .ok();
                let joined = join_reasoning_summary(&summary);
                if let Some(AssistantContent::Thinking(t)) =
                    self.partial.content.get_mut(content_index)
                {
                    t.thinking = joined.clone();
                    t.thinking_signature = signature;
                }
                out.push(AssistantMessageEvent::ThinkingEnd {
                    content_index,
                    content: joined,
                    partial: self.partial.clone(),
                });
            }
            (
                ResponseOutputItem::Message { id, phase, .. },
                Some(ItemSlot::Message { text_blocks, .. }),
            ) => {
                let signature = serialize_text_signature(id, phase);
                let mut indices: Vec<(u32, usize)> = text_blocks.into_iter().collect();
                indices.sort_by_key(|(part_idx, _)| *part_idx);
                for (_, content_index) in indices {
                    let mut text_clone = String::new();
                    if let Some(AssistantContent::Text(t)) =
                        self.partial.content.get_mut(content_index)
                    {
                        t.text_signature = signature.clone();
                        text_clone = t.text.clone();
                    }
                    out.push(AssistantMessageEvent::TextEnd {
                        content_index,
                        content: text_clone,
                        partial: self.partial.clone(),
                    });
                }
            }
            (
                ResponseOutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                    ..
                },
                Some(ItemSlot::FunctionCall { content_index, .. }),
            ) => {
                // The terminal `arguments` string from the wire wins
                // over the streaming buffer — it's always the
                // canonical, complete payload.
                let parsed: Value = if arguments.is_empty() {
                    Value::Object(serde_json::Map::new())
                } else {
                    serde_json::from_str(&arguments)
                        .unwrap_or_else(|_| parse_streaming_json(&arguments))
                };
                let composite = compose_tool_use_id(&call_id, id.as_deref());
                let mut snapshot = None;
                if let Some(AssistantContent::ToolCall(tc)) =
                    self.partial.content.get_mut(content_index)
                {
                    tc.id = composite;
                    tc.name = name;
                    tc.arguments = parsed;
                    snapshot = Some(tc.clone());
                }
                if let Some(tool_call) = snapshot {
                    out.push(AssistantMessageEvent::ToolCallEnd {
                        content_index,
                        tool_call,
                        partial: self.partial.clone(),
                    });
                }
            }
            _ => {}
        }
    }

    fn on_text_delta(
        &mut self,
        output_index: u32,
        content_index: u32,
        delta: String,
        out: &mut Vec<AssistantMessageEvent>,
    ) {
        let Some(slot) = self.slots.get_mut(&output_index) else {
            return;
        };
        let ItemSlot::Message { text_blocks, .. } = slot else {
            return;
        };
        let (idx, is_new) = match text_blocks.get(&content_index).copied() {
            Some(idx) => (idx, false),
            None => {
                let idx = self.partial.content.len();
                text_blocks.insert(content_index, idx);
                (idx, true)
            }
        };
        if is_new {
            self.partial
                .content
                .push(AssistantContent::Text(TextContent {
                    text: String::new(),
                    text_signature: None,
                }));
            out.push(AssistantMessageEvent::TextStart {
                content_index: idx,
                partial: self.partial.clone(),
            });
        }
        if let Some(AssistantContent::Text(t)) = self.partial.content.get_mut(idx) {
            t.text.push_str(&delta);
        }
        out.push(AssistantMessageEvent::TextDelta {
            content_index: idx,
            delta,
            partial: self.partial.clone(),
        });
    }

    fn on_function_args_delta(
        &mut self,
        output_index: u32,
        delta: &str,
        out: &mut Vec<AssistantMessageEvent>,
    ) {
        let Some(ItemSlot::FunctionCall {
            content_index,
            arguments,
            ..
        }) = self.slots.get_mut(&output_index)
        else {
            return;
        };
        arguments.push_str(delta);
        let parsed = parse_streaming_json(arguments);
        let idx = *content_index;
        if let Some(AssistantContent::ToolCall(tc)) = self.partial.content.get_mut(idx) {
            tc.arguments = parsed;
        }
        out.push(AssistantMessageEvent::ToolCallDelta {
            content_index: idx,
            delta: delta.to_string(),
            partial: self.partial.clone(),
        });
    }

    fn on_reasoning_summary_part_added(
        &mut self,
        output_index: u32,
        out: &mut Vec<AssistantMessageEvent>,
    ) {
        let Some(ItemSlot::Reasoning {
            content_index,
            seen_part_count,
            ..
        }) = self.slots.get_mut(&output_index)
        else {
            return;
        };
        let idx = *content_index;
        let was_first = *seen_part_count == 0;
        *seen_part_count += 1;
        if was_first {
            return;
        }
        // §7.3.6: emit a "\n\n" separator on the second-and-later parts.
        if let Some(AssistantContent::Thinking(t)) = self.partial.content.get_mut(idx) {
            t.thinking.push_str("\n\n");
        }
        out.push(AssistantMessageEvent::ThinkingDelta {
            content_index: idx,
            delta: "\n\n".to_string(),
            partial: self.partial.clone(),
        });
    }

    fn on_reasoning_delta(
        &mut self,
        output_index: u32,
        delta: &str,
        out: &mut Vec<AssistantMessageEvent>,
    ) {
        let Some(ItemSlot::Reasoning { content_index, .. }) = self.slots.get_mut(&output_index)
        else {
            return;
        };
        let idx = *content_index;
        if let Some(AssistantContent::Thinking(t)) = self.partial.content.get_mut(idx) {
            t.thinking.push_str(delta);
        }
        out.push(AssistantMessageEvent::ThinkingDelta {
            content_index: idx,
            delta: delta.to_string(),
            partial: self.partial.clone(),
        });
    }

    pub(super) fn finalize(mut self) -> AssistantMessageEvent {
        // Apply usage / cost from the captured terminal response.
        let server_tier = self
            .final_response
            .as_ref()
            .and_then(|r| r.service_tier.clone());
        let multiplier = (self.cost_multiplier)(
            &self.partial.model,
            server_tier.as_ref(),
            self.requested_tier.as_ref(),
        );
        if let Some(usage) = self.final_response.as_ref().and_then(|r| r.usage.as_ref()) {
            apply_usage(&mut self.partial.usage, usage);
        }
        let cost_model = model_for_cost(&self.partial);
        finalize_usage(&mut self.partial.usage, &cost_model, multiplier);

        // Classify the terminal status.
        let has_tool_use = self
            .partial
            .content
            .iter()
            .any(|b| matches!(b, AssistantContent::ToolCall(_)));
        let (stop_reason, done_reason, error_detail) = classify_status(
            self.finish_status.as_ref(),
            self.final_response
                .as_ref()
                .and_then(|r| r.incomplete_details.as_ref())
                .and_then(|d| d.reason.as_deref()),
            has_tool_use,
            self.finish_error.take(),
            self.api_name,
        );
        self.partial.stop_reason = stop_reason;

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
                        "{}: terminated without recognized status ({:?})",
                        self.api_name, self.finish_status
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

fn classify_status(
    status: Option<&ResponseStatus>,
    incomplete_reason: Option<&str>,
    has_tool_use: bool,
    error: Option<AssistantError>,
    api_name: &str,
) -> (StopReason, Option<DoneReason>, Option<AssistantError>) {
    match status {
        Some(ResponseStatus::Completed) | None if has_tool_use => {
            (StopReason::ToolUse, Some(DoneReason::ToolUse), None)
        }
        Some(ResponseStatus::Completed) | None => (StopReason::Stop, Some(DoneReason::Stop), None),
        Some(ResponseStatus::Incomplete) => match incomplete_reason {
            Some("max_output_tokens") | Some("length") => {
                (StopReason::Length, Some(DoneReason::Length), None)
            }
            Some("max_tool_calls") => (StopReason::ToolUse, Some(DoneReason::ToolUse), None),
            Some("content_filter") => (
                StopReason::Error,
                None,
                Some(error.unwrap_or_else(|| {
                    AssistantError::new(ErrorCategory::ContentFilter, "Incomplete: content_filter")
                })),
            ),
            // §7.3.8 safe default — treat unknown / missing reason as
            // a length cutoff.
            _ => (StopReason::Length, Some(DoneReason::Length), None),
        },
        Some(ResponseStatus::Failed) | Some(ResponseStatus::Cancelled) => (
            StopReason::Error,
            None,
            Some(error.unwrap_or_else(|| {
                AssistantError::new(
                    ErrorCategory::Unknown,
                    format!("{}: response status {:?}", api_name, status),
                )
            })),
        ),
        // §7.3.8: in_progress / queued shouldn't appear on a finished
        // response; handle defensively as Stop.
        Some(ResponseStatus::InProgress) | Some(ResponseStatus::Queued) => {
            (StopReason::Stop, Some(DoneReason::Stop), None)
        }
    }
}

pub(super) fn error_from_response(response: &Response) -> AssistantError {
    if let Some(err) = &response.error {
        return error_from_code(Some(err.code.as_str()), err.message.clone());
    }
    let message = response
        .incomplete_details
        .as_ref()
        .and_then(|d| d.reason.clone())
        .unwrap_or_else(|| "openai-responses: response failed".to_string());
    AssistantError::new(ErrorCategory::Unknown, message)
}

pub(super) fn error_from_code(code: Option<&str>, message: String) -> AssistantError {
    classify_openai_error(code, None, None, None, message)
}

// ---------------------------------------------------------------------------
// Usage merging + cost (§7.3.7)
// ---------------------------------------------------------------------------

fn apply_usage(target: &mut crate::types::Usage, source: &ResponseUsage) {
    let cached = source
        .input_tokens_details
        .as_ref()
        .and_then(|d| d.cached_tokens)
        .map(u64::from)
        .unwrap_or(0);
    let prompt = u64::from(source.input_tokens);
    target.cache_read = cached;
    target.cache_write = 0; // §7.3.7: Responses doesn't report cache writes.
    target.input = prompt.saturating_sub(cached);
    target.output = u64::from(source.output_tokens);
}

fn finalize_usage(usage: &mut crate::types::Usage, model: &ModelInfo, tier_multiplier: f64) {
    usage.total_tokens = usage.input + usage.output + usage.cache_read + usage.cache_write;
    calculate_cost(model, usage);
    if (tier_multiplier - 1.0).abs() > f64::EPSILON {
        usage.cost.input *= tier_multiplier;
        usage.cost.output *= tier_multiplier;
        usage.cost.cache_read *= tier_multiplier;
        usage.cost.cache_write *= tier_multiplier;
        usage.cost.total *= tier_multiplier;
    }
}

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{InputModality, ModelCost};
    use crate::types::{Message as UnifiedMessage, UserContent, UserMessage};

    fn fake_model(reasoning: bool) -> ModelInfo {
        ModelInfo {
            id: "gpt-5".into(),
            name: "GPT-5".into(),
            api: API_NAME.into(),
            provider: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            reasoning,
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

    #[test]
    fn build_request_omits_reasoning_on_non_reasoning_models() {
        let ctx = Context::new("hello");
        let req = build_request(
            &fake_model(false),
            &ctx,
            &StreamOptions::default(),
            Some(&ThinkingLevel::High),
        );
        assert!(req.reasoning.is_none());
        assert!(req.include.is_empty());
    }

    #[test]
    fn build_request_sets_include_and_summary_when_reasoning() {
        let ctx = Context::new("hello");
        let req = build_request(
            &fake_model(true),
            &ctx,
            &StreamOptions::default(),
            Some(&ThinkingLevel::Medium),
        );
        let r = req.reasoning.expect("reasoning set");
        assert!(matches!(r.effort, Some(ReasoningEffort::Medium)));
        assert!(matches!(r.summary, Some(ReasoningSummaryMode::Auto)));
        assert_eq!(
            req.include,
            vec![ResponseIncludable::ReasoningEncryptedContent]
        );
        assert_eq!(req.store, Some(false));
    }

    #[test]
    fn build_request_reasoning_disabled_uses_effort_none_and_no_include() {
        let ctx = Context::new("hello");
        let req = build_request(&fake_model(true), &ctx, &StreamOptions::default(), None);
        let r = req.reasoning.expect("reasoning set");
        assert!(matches!(r.effort, Some(ReasoningEffort::None)));
        assert!(req.include.is_empty());
    }

    #[test]
    fn build_request_prompt_cache_key_and_retention() {
        let ctx = Context::new("hello");
        let opts = StreamOptions {
            session_id: Some("sid".into()),
            cache_retention: CacheRetention::Long,
            ..Default::default()
        };
        let req = build_request(&fake_model(false), &ctx, &opts, None);
        assert_eq!(req.prompt_cache_key.as_deref(), Some("sid"));
        assert!(matches!(
            req.prompt_cache_retention,
            Some(PromptCacheRetention::TwentyFourHours)
        ));
    }

    #[test]
    fn build_request_no_cache_when_retention_none() {
        let ctx = Context::new("hello");
        let opts = StreamOptions {
            session_id: Some("sid".into()),
            cache_retention: CacheRetention::None,
            ..Default::default()
        };
        let req = build_request(&fake_model(false), &ctx, &opts, None);
        assert!(req.prompt_cache_key.is_none());
        assert!(req.prompt_cache_retention.is_none());
    }

    #[test]
    fn cross_model_tool_call_drops_item_id() {
        let mut m = AssistantMessage::empty();
        m.api = "anthropic-messages".into();
        m.content.push(AssistantContent::ToolCall(ToolCall {
            id: "call_x|fc_y".into(),
            name: "ls".into(),
            arguments: serde_json::json!({}),
        }));
        let items = assistant_message_to_input_items(&m);
        match &items[0] {
            ResponseInputItem::FunctionCall { id, call_id, .. } => {
                assert_eq!(call_id, "call_x");
                assert!(id.is_none(), "cross-model item_id should be omitted");
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn same_model_tool_call_preserves_composite_id() {
        let mut m = AssistantMessage::empty();
        m.api = API_NAME.into();
        m.content.push(AssistantContent::ToolCall(ToolCall {
            id: "call_x|fc_y".into(),
            name: "ls".into(),
            arguments: serde_json::json!({}),
        }));
        let items = assistant_message_to_input_items(&m);
        match &items[0] {
            ResponseInputItem::FunctionCall { id, call_id, .. } => {
                assert_eq!(call_id, "call_x");
                assert_eq!(id.as_deref(), Some("fc_y"));
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn assistant_with_signed_text_emits_message_with_id_phase() {
        let mut m = AssistantMessage::empty();
        m.api = API_NAME.into();
        let sig = serialize_text_signature("msg_abc".into(), Some(MessagePhase::FinalAnswer));
        m.content.push(AssistantContent::Text(TextContent {
            text: "hello".into(),
            text_signature: sig,
        }));
        let items = assistant_message_to_input_items(&m);
        match &items[0] {
            ResponseInputItem::Message { id, phase, .. } => {
                assert_eq!(id.as_deref(), Some("msg_abc"));
                assert_eq!(phase.as_ref(), Some(&MessagePhase::FinalAnswer));
            }
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn user_image_serializes_as_data_url() {
        let user = UserMessage {
            content: vec![UserContent::image("Zm9v", "image/png")],
            timestamp: 0,
        };
        let mut out = Vec::new();
        convert_messages(API_NAME, &[UnifiedMessage::User(user)], &mut out);
        match &out[0] {
            ResponseInputItem::Message { content, .. } => match content {
                ResponseInputMessageContent::Array(parts) => {
                    assert!(matches!(
                        &parts[0],
                        ResponseInputContentPart::InputImage { image_url: Some(u), .. }
                            if u == "data:image/png;base64,Zm9v"
                    ));
                }
                _ => panic!("unexpected content"),
            },
            other => panic!("unexpected item: {other:?}"),
        }
    }

    #[test]
    fn classify_status_completed_with_tool_use() {
        let (sr, dr, _) =
            classify_status(Some(&ResponseStatus::Completed), None, true, None, API_NAME);
        assert_eq!(sr, StopReason::ToolUse);
        assert_eq!(dr, Some(DoneReason::ToolUse));
    }

    #[test]
    fn classify_status_incomplete_subcases() {
        let (sr, dr, _) = classify_status(
            Some(&ResponseStatus::Incomplete),
            Some("max_output_tokens"),
            false,
            None,
            API_NAME,
        );
        assert_eq!(sr, StopReason::Length);
        assert_eq!(dr, Some(DoneReason::Length));

        let (sr, dr, _) = classify_status(
            Some(&ResponseStatus::Incomplete),
            Some("max_tool_calls"),
            false,
            None,
            API_NAME,
        );
        assert_eq!(sr, StopReason::ToolUse);
        assert_eq!(dr, Some(DoneReason::ToolUse));

        let (sr, dr, err) = classify_status(
            Some(&ResponseStatus::Incomplete),
            Some("content_filter"),
            false,
            None,
            API_NAME,
        );
        assert_eq!(sr, StopReason::Error);
        assert!(dr.is_none());
        assert_eq!(err.unwrap().category, ErrorCategory::ContentFilter);

        let (sr, dr, _) = classify_status(
            Some(&ResponseStatus::Incomplete),
            None,
            false,
            None,
            API_NAME,
        );
        assert_eq!(sr, StopReason::Length);
        assert!(dr.is_some());
    }

    #[test]
    fn cost_multiplier_applied() {
        let mut m = AssistantMessage::empty();
        m.api = API_NAME.into();
        m.provider = "openai".into();
        m.model = "gpt-5".into();
        let mut state = StreamState::new(&fake_model(false), Some(ServiceTier::Flex));
        state.partial = m;
        // No usage report — multiplier still applied (to zero, no-op).
        let event = state.finalize();
        let msg = match event {
            AssistantMessageEvent::Done { message, .. } => message,
            other => panic!("expected Done, got {other:?}"),
        };
        assert_eq!(msg.usage.cost.total, 0.0);
    }

    #[test]
    fn is_openai_host_check() {
        assert!(is_openai_host("https://api.openai.com/v1"));
        assert!(is_openai_host("https://api.openai.com"));
        assert!(!is_openai_host("https://oai.azure.com/v1"));
        assert!(!is_openai_host("http://localhost:8080/v1"));
    }

    #[test]
    fn parse_text_signature_v1_round_trip() {
        let sig = serialize_text_signature("msg_x".into(), Some(MessagePhase::Commentary)).unwrap();
        let parsed = parse_text_signature(Some(&sig));
        assert_eq!(parsed.id.as_deref(), Some("msg_x"));
        assert_eq!(parsed.phase, Some(MessagePhase::Commentary));
    }

    #[test]
    fn parse_text_signature_legacy_plain_id() {
        let parsed = parse_text_signature(Some("legacy_id"));
        assert_eq!(parsed.id.as_deref(), Some("legacy_id"));
        assert!(parsed.phase.is_none());
    }
}
