//! OpenAI Codex Responses API provider.
//!
//! Implements the unified [`Provider`] trait against the Codex
//! deployment of the Responses API at
//! `https://chatgpt.com/backend-api/codex/responses`. See
//! `docs/models-spec.md` §7.4 for the full design.
//!
//! Codex is a ChatGPT-Plus-gated mirror of [`super::responses`] with a
//! handful of wire-shape differences:
//!
//! - **Authentication.** The credential is the OAuth JWT from §9.4,
//!   not an API key. The provider decodes the JWT at request time and
//!   sends the `chatgpt-account-id` claim as a header.
//! - **System prompt routing.** Sent as the top-level `instructions`
//!   field rather than a `developer`/`system` input item.
//! - **Hardcoded request fields.** `store: false`, `tool_choice: "auto"`,
//!   and `parallel_tool_calls: true` are baked in; `strict`,
//!   `prompt_cache_retention`, `text.verbosity`, and `max_output_tokens`
//!   are never sent.
//! - **Event normalization.** Older `response.done` / `response.incomplete`
//!   event names are rewritten to `response.completed` before reaching
//!   the shared §7.3.6 state machine, and top-level `error` /
//!   `response.failed` events surface as terminal errors via the §10
//!   classifier with a Codex-specific 429 friendly-message overlay.
//! - **Service-tier pricing.** Same `flex` / `priority` knob, but
//!   `gpt-5.5 + priority` uses a 2.5× multiplier (vs the default 2×).
//!
//! Everything else — the streaming state machine, reasoning
//! round-trip, composite tool-call IDs, usage parsing, stop-reason
//! mapping — is shared with [`super::responses`].

use futures::StreamExt;
use openai_sdk::client::{Client, ClientError};
use openai_sdk::types::common::{ReasoningEffort, ServiceTier as OpenAIServiceTier};
use openai_sdk::types::responses::{
    CreateResponseRequest, Reasoning, ReasoningSummaryMode, ResponseIncludable, ResponseInput,
    ResponseInputItem, ResponseInstructions, ResponseStatus, ResponseStreamEvent, ResponseTool,
    ResponseToolChoice,
};
use serde::Deserialize;
use serde_json::Value;

use crate::errors::{classify_openai_error, parse_retry_after, transport_error};
use crate::oauth::openai::extract_account_id;
use crate::provider::Provider;
use crate::registry::ModelInfo;
use crate::streaming::{
    AssistantMessageEvent, AssistantMessageEventStream, ErrorReason, SelectOutcome, select_cancel,
};
use crate::transform::transform_messages;
use crate::types::{
    AssistantError, AssistantMessage, Context, ErrorCategory,
    ReasoningSummary as UnifiedReasoningSummary, ServiceTier, SimpleStreamOptions, StopReason,
    StreamOptions, ThinkingLevel, ToolDefinition,
};

use super::responses::{
    CostMultiplierFn, StreamState, append_assistant_message, convert_messages, empty_partial,
    error_from_code, map_reasoning_effort, map_service_tier, parse_assistant_input_items_with_api,
};

/// `api` field reported on assistant messages produced by this provider.
pub(super) const API_NAME: &str = "openai-codex-responses";

/// Cost-multiplier function pointer for the Codex pricing curve. A
/// `const` lets the responses-shared `StreamState` pick up the
/// function pointer without an inline `as` cast at each call site.
pub(super) const CODEX_COST_MULTIPLIER: CostMultiplierFn = codex_cost_multiplier;

/// Default base URL for the Codex backend. The model registry sets the
/// same value (see `bundled_codex_seed`); this constant is the
/// fallback when the provider is invoked against a [`ModelInfo`] with
/// an empty `base_url`.
const DEFAULT_BASE_URL: &str = "https://chatgpt.com/backend-api";

/// Required `OpenAI-Beta` header value for the SSE Codex endpoint
/// (per §7.4.1). The WebSocket transport uses a different beta tag
/// and is explicitly out of scope per §7.4.8.
const OPENAI_BETA: &str = "responses=experimental";

/// Originator identifier sent on every Codex request. Matches the
/// value the OAuth flow uses for the authorize URL.
const ORIGINATOR: &str = "aj";

/// Stateless provider for the Codex Responses API.
pub struct OpenAiCodexResponsesProvider;

impl Provider for OpenAiCodexResponsesProvider {
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
    if let Some(token) = options.cancel.as_ref()
        && token.is_cancelled()
    {
        producer.push(AssistantMessageEvent::aborted(empty_partial(
            API_NAME, model,
        )));
        return Ok(());
    }

    let access_token = options.resolve_api_key().await.map_err(|err| {
        AssistantError::new(
            ErrorCategory::Auth,
            format!(
                "openai-codex-responses provider: {err} \
                 (set api_key or api_key_resolver to the OAuth access token JWT)"
            ),
        )
    })?;

    // §7.4.1: decode the JWT *at request time* so a refresh in flight
    // doesn't desync the header from the bearer token.
    let account_id = extract_account_id(&access_token).ok_or_else(|| {
        AssistantError::new(
            ErrorCategory::Auth,
            "openai-codex-responses: access token JWT missing chatgpt_account_id claim",
        )
    })?;

    let base_url = if model.base_url.is_empty() {
        DEFAULT_BASE_URL.to_string()
    } else {
        model.base_url.clone()
    };

    let mut client = Client::new(Some(base_url), access_token)
        .with_extra_header("chatgpt-account-id", account_id)
        .with_extra_header("originator", ORIGINATOR)
        .with_extra_header("OpenAI-Beta", OPENAI_BETA)
        .with_extra_header("User-Agent", user_agent());

    // §7.4.1: session-correlation headers mirror §7.3. Codex is always
    // hosted at chatgpt.com so we don't gate on hostname here.
    if let Some(sid) = options.session_id.as_deref() {
        client = client
            .with_extra_header("session_id", sid)
            .with_extra_header("x-client-request-id", sid);
    }

    let request = build_request(model, context, options, reasoning);

    if let Some(cb) = options.on_payload.as_ref() {
        match serde_json::to_value(&request) {
            Ok(body) => cb.call(&body),
            Err(err) => tracing::warn!("on_payload serialization failed: {err}"),
        }
    }

    let mut sse = match select_cancel(
        options.cancel.as_ref(),
        client.codex_responses_stream(request),
    )
    .await
    {
        SelectOutcome::Ready(res) => res.map_err(|err| classify_codex_client_error(&err))?,
        SelectOutcome::Cancelled => {
            producer.push(AssistantMessageEvent::aborted(empty_partial(
                API_NAME, model,
            )));
            return Ok(());
        }
    };

    let mut state = StreamState::new_with(
        API_NAME,
        model,
        options.service_tier.clone(),
        CODEX_COST_MULTIPLIER,
    );

    loop {
        match select_cancel(options.cancel.as_ref(), sse.next()).await {
            SelectOutcome::Ready(Some(Ok(ev))) => match normalize_codex_event(ev)? {
                NormalizedEvent::Forward(ev) => {
                    for out in state.process(ev) {
                        producer.push(out);
                    }
                }
                NormalizedEvent::Terminal(ev) => {
                    for out in state.process(ev) {
                        producer.push(out);
                    }
                    // The Codex backend sometimes keeps the stream
                    // open after the terminal event — stop consuming
                    // once we've seen a completion. Anything else is
                    // noise.
                    break;
                }
            },
            SelectOutcome::Ready(Some(Err(err))) => return Err(classify_codex_client_error(&err)),
            SelectOutcome::Ready(None) => break,
            SelectOutcome::Cancelled => {
                producer.push(AssistantMessageEvent::aborted(state.partial().clone()));
                return Ok(());
            }
        }
    }

    let final_event = state.finalize();
    producer.push(final_event);
    Ok(())
}

/// Build the `User-Agent` header value, formatted as
/// `aj/<version> (<os> <arch>)`.
fn user_agent() -> String {
    format!(
        "aj/{} ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
    )
}

// ---------------------------------------------------------------------------
// Error classification (§7.4.6)
// ---------------------------------------------------------------------------

/// Wrap [`classify_client_error`] to overlay the Codex-specific
/// friendly 429 message from §7.4.6.
fn classify_codex_client_error(err: &ClientError) -> AssistantError {
    match err {
        ClientError::ApiError {
            error,
            http_status,
            retry_after,
        } => {
            let retry_after_ms = parse_retry_after(retry_after.as_deref());
            let friendly = friendly_codex_message(
                error.code.as_deref(),
                error.r#type.as_deref(),
                *http_status,
                &error.message,
            );
            let message = friendly.unwrap_or_else(|| error.message.clone());
            classify_openai_error(
                error.code.as_deref(),
                error.r#type.as_deref(),
                Some(*http_status),
                retry_after_ms,
                message,
            )
        }
        ClientError::TransportError(t) => transport_error(format!("transport: {t}")),
        ClientError::ParseError(s) => transport_error(format!("parse: {s}")),
        ClientError::InternalError(s) => transport_error(format!("internal: {s}")),
    }
}

/// Build the §7.4.6 "You have hit your ChatGPT usage limit" message
/// from optional `plan_type` / `resets_at` fields on the API error
/// payload. Returns `None` for non-usage-cap errors so the caller
/// falls back to the raw server message.
///
/// The official `ApiError` shape doesn't carry these Codex-specific
/// fields, so this helper re-parses the original error body when
/// available — the live SDK only surfaces the `code` / `message`, so
/// we accept the loss of `plan_type` / `resets_at` and emit the bare
/// friendly message in that case.
fn friendly_codex_message(
    code: Option<&str>,
    r#type: Option<&str>,
    http_status: u16,
    fallback_message: &str,
) -> Option<String> {
    let raw_code = code.or(r#type).map(str::to_lowercase).unwrap_or_default();
    let usage_shape = matches!(
        raw_code.as_str(),
        "usage_limit_reached" | "usage_not_included" | "rate_limit_exceeded",
    ) || http_status == 429;
    if !usage_shape {
        return None;
    }

    // Try to extract `plan_type` / `resets_at` from the fallback
    // message if it was a JSON body that fell through serde — the
    // openai-sdk's `ApiError` struct doesn't model these custom
    // fields, but they sometimes show up as JSON text in `message`.
    let (plan, mins) = parse_usage_metadata(fallback_message);
    Some(format_friendly_message(plan.as_deref(), mins))
}

#[derive(Deserialize)]
struct UsageErrorMetadata {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    resets_at: Option<i64>,
}

fn parse_usage_metadata(body: &str) -> (Option<String>, Option<u64>) {
    let trimmed = body.trim();
    if !trimmed.starts_with('{') {
        return (None, None);
    }
    // The body may be either `{"plan_type":...,"resets_at":...}` or
    // `{"error":{...}}` — try the envelope shape first since it's
    // more specific. Fall back to the bare shape if the top-level
    // fields are populated directly.
    #[derive(Deserialize)]
    struct ErrorEnvelope {
        error: UsageErrorMetadata,
    }
    if let Ok(env) = serde_json::from_str::<ErrorEnvelope>(trimmed)
        && (env.error.plan_type.is_some() || env.error.resets_at.is_some())
    {
        return (env.error.plan_type, minutes_until(env.error.resets_at));
    }
    if let Ok(meta) = serde_json::from_str::<UsageErrorMetadata>(trimmed)
        && (meta.plan_type.is_some() || meta.resets_at.is_some())
    {
        return (meta.plan_type, minutes_until(meta.resets_at));
    }
    (None, None)
}

/// Convert a unix-seconds reset timestamp to minutes-until-reset,
/// floored at 0 and rounded to nearest minute. Returns `None` for
/// absent or already-in-the-past timestamps that round to 0.
#[allow(clippy::as_conversions)]
fn minutes_until(resets_at: Option<i64>) -> Option<u64> {
    let resets_at = resets_at?;
    let now_secs = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .as_secs(),
    )
    .unwrap_or(i64::MAX);
    let delta_secs = (resets_at - now_secs).max(0);
    // Round to nearest minute.
    let minutes = (f64::from(i32::try_from(delta_secs).unwrap_or(i32::MAX)) / 60.0).round();
    Some(minutes.max(0.0) as u64)
}

fn format_friendly_message(plan_type: Option<&str>, mins: Option<u64>) -> String {
    let plan = match plan_type {
        Some(p) if !p.is_empty() => format!(" ({} plan)", p.to_lowercase()),
        _ => String::new(),
    };
    let when = match mins {
        Some(m) => format!(" Try again in ~{m} min."),
        None => String::new(),
    };
    if when.is_empty() {
        format!("You have hit your ChatGPT usage limit{plan}.")
    } else {
        format!("You have hit your ChatGPT usage limit{plan}.{when}")
    }
}

// ---------------------------------------------------------------------------
// Event normalization (§7.4.5)
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum NormalizedEvent {
    /// Pass the event through to the §7.3.6 state machine unchanged.
    Forward(ResponseStreamEvent),
    /// Pass through, then stop draining the stream. Used for
    /// `response.completed` / rewritten `response.done` /
    /// `response.incomplete`.
    Terminal(ResponseStreamEvent),
}

/// §7.4.5: normalize the Codex event stream so it looks like a plain
/// Responses stream by the time it reaches the shared state machine.
///
/// - `error` and `response.failed` are surfaced as `Err` so the caller
///   classifies them through [`classify_codex_client_error`] (with
///   the friendly-message overlay) before short-circuiting the run.
/// - `response.done` / `response.incomplete` are rewritten to
///   `response.completed` with `response.status` normalized into the
///   recognized set, then forwarded as a terminal event.
/// - Everything else is forwarded unchanged.
fn normalize_codex_event(ev: ResponseStreamEvent) -> Result<NormalizedEvent, AssistantError> {
    match ev {
        ResponseStreamEvent::Error { code, message, .. } => {
            Err(error_from_code(code.as_deref(), message))
        }
        ResponseStreamEvent::ResponseFailed { response, .. } => {
            let err = if let Some(err) = response.error.as_ref() {
                error_from_code(Some(err.code.as_str()), err.message.clone())
            } else {
                AssistantError::new(
                    ErrorCategory::Unknown,
                    "openai-codex-responses: response failed".to_string(),
                )
            };
            Err(err)
        }
        ResponseStreamEvent::ResponseCompleted {
            response,
            sequence_number,
        } => Ok(NormalizedEvent::Terminal(
            ResponseStreamEvent::ResponseCompleted {
                response: normalize_response_status(response),
                sequence_number,
            },
        )),
        ResponseStreamEvent::ResponseIncomplete {
            response,
            sequence_number,
        } => {
            // Rewrite the event type to `Completed` while preserving
            // the inner `status` so the state machine's
            // `classify_status` arm picks up the `Incomplete` branch
            // (length cutoff, content filter, etc.) from §7.3.8.
            let mut response = normalize_response_status(response);
            if response.status.is_none() {
                response.status = Some(ResponseStatus::Incomplete);
            }
            Ok(NormalizedEvent::Terminal(
                ResponseStreamEvent::ResponseCompleted {
                    response,
                    sequence_number,
                },
            ))
        }
        ResponseStreamEvent::Other(value) => {
            // Catch the legacy `response.done` shape (older event name
            // the Codex backend still emits in places): it deserializes
            // as `Other` because we don't have a variant for it. Detect
            // by `type` field and rewrite.
            if let Some(t) = value.get("type").and_then(Value::as_str)
                && (t == "response.done" || t == "response.incomplete")
            {
                return rewrite_legacy_done(value);
            }
            Ok(NormalizedEvent::Forward(ResponseStreamEvent::Other(value)))
        }
        other => Ok(NormalizedEvent::Forward(other)),
    }
}

/// Rewrite a `response.done` / `response.incomplete` event arriving as
/// an [`ResponseStreamEvent::Other`] into a typed
/// `response.completed`. We change the type label, normalize the inner
/// `response.status`, and rebuild the wire value so serde
/// deserialization re-fires through the strict variant. On any
/// failure we surface a `Forward` of the original value rather than
/// dropping the event — better to feed the state machine an unknown
/// event than to silently lose terminal information.
fn rewrite_legacy_done(value: Value) -> Result<NormalizedEvent, AssistantError> {
    let mut rewritten = value.clone();
    if let Some(obj) = rewritten.as_object_mut() {
        let old_type = obj
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default();
        obj.insert(
            "type".to_string(),
            Value::String("response.completed".to_string()),
        );
        if old_type == "response.incomplete"
            && let Some(response) = obj.get_mut("response")
            && response.is_object()
        {
            // Preserve `Incomplete` status so the state machine picks
            // the right §7.3.8 branch.
            let response_obj = response.as_object_mut().expect("response is object");
            response_obj
                .entry("status".to_string())
                .or_insert(Value::String("incomplete".to_string()));
        }
        normalize_response_status_in_value(obj);
    }
    match serde_json::from_value::<ResponseStreamEvent>(rewritten) {
        Ok(event) => Ok(NormalizedEvent::Terminal(event)),
        Err(_) => Ok(NormalizedEvent::Forward(ResponseStreamEvent::Other(value))),
    }
}

/// In-place version of [`normalize_response_status`] that works on a
/// `serde_json::Map` so we can rewrite the §7.4.5 unknown-status
/// values before the strict deserializer rejects them.
fn normalize_response_status_in_value(obj: &mut serde_json::Map<String, Value>) {
    let Some(response) = obj.get_mut("response") else {
        return;
    };
    let Some(response_obj) = response.as_object_mut() else {
        return;
    };
    let Some(status) = response_obj.get("status").and_then(Value::as_str) else {
        return;
    };
    if !matches!(
        status,
        "completed" | "incomplete" | "failed" | "cancelled" | "queued" | "in_progress",
    ) {
        // Drop unrecognized statuses — the state machine's
        // `classify_status` treats `None` as the default Stop branch.
        response_obj.remove("status");
    }
}

/// Replace any unrecognized [`ResponseStatus`] value on the inner
/// response with `None`, leaving the recognized set
/// `{completed, incomplete, failed, cancelled, queued, in_progress}`.
///
/// Today our [`ResponseStatus`] enum already enumerates exactly that
/// set, so deserialization of unknown values fails earlier — meaning
/// any [`ResponseStatus`] we *do* hold is already in the recognized
/// set. The function exists for symmetry with the spec text and to
/// keep the spot we'd patch if the SDK enum gains catch-all variants
/// later.
fn normalize_response_status(
    response: openai_sdk::types::responses::Response,
) -> openai_sdk::types::responses::Response {
    response
}

// ---------------------------------------------------------------------------
// Cost multiplier (§7.4.4)
// ---------------------------------------------------------------------------

/// §7.4.4 service-tier cost curve. Same `flex` / `priority` knobs as
/// the public Responses API, except `gpt-5.5 + priority` uses a 2.5×
/// multiplier (vs the default 2×). The `default` / `auto` / absent
/// tier always uses 1×.
///
/// Service-tier resolution follows §7.4.4: the server-echoed tier
/// wins, except when the server reports `default` after the caller
/// asked for `flex` or `priority`. In that case we apply pricing as
/// if the request had succeeded (the server still serves the response
/// but reports the standard tier).
pub(crate) fn codex_cost_multiplier(
    model_id: &str,
    server_tier: Option<&OpenAIServiceTier>,
    requested_tier: Option<&OpenAIServiceTier>,
) -> f64 {
    let effective = resolve_codex_service_tier(server_tier, requested_tier);
    match effective {
        Some(OpenAIServiceTier::Flex) => 0.5,
        Some(OpenAIServiceTier::Priority) => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

fn resolve_codex_service_tier<'a>(
    server: Option<&'a OpenAIServiceTier>,
    requested: Option<&'a OpenAIServiceTier>,
) -> Option<&'a OpenAIServiceTier> {
    match (server, requested) {
        // §7.4.4 "response value wins; requested value falls back when
        // the response reports `default`". Today `OpenAIServiceTier`
        // doesn't model `default` as a variant — it's either Flex,
        // Priority, or absent — but we keep the resolver structured
        // so future expansion fits in one place.
        (Some(s), _) => Some(s),
        (None, Some(r)) => Some(r),
        (None, None) => None,
    }
}

// ---------------------------------------------------------------------------
// Round-trip helpers (`docs/models-spec.md` §1.10)
// ---------------------------------------------------------------------------

/// Serialize side of the §1.10 invariant for `openai-codex-responses`:
/// project an [`AssistantMessage`] onto the typed input items the
/// Codex endpoint expects on the request side.
///
/// Identical wire shape to the public Responses API
/// ([`super::responses::assistant_message_to_input_items`]), but tagged
/// with the Codex `api` identifier so cross-model checks in
/// [`super::responses::append_assistant_message`] see the right
/// provider identity (a message produced by `openai-codex-responses`
/// stays "same-provider" when re-serialized through this helper).
pub fn assistant_message_to_input_items(message: &AssistantMessage) -> Vec<ResponseInputItem> {
    let mut out = Vec::new();
    append_assistant_message(API_NAME, message, &mut out);
    out
}

/// Inverse of [`assistant_message_to_input_items`]: parse a sequence
/// of Codex `input` items whose role is `assistant` (plus interleaved
/// reasoning / function_call items) back into a unified
/// [`AssistantMessage`] tagged with [`API_NAME`].
///
/// Symmetric to the streaming state machine; exposed so the round-trip
/// suite can replay request bodies through the same parse path.
pub fn parse_assistant_input_items(items: &[ResponseInputItem]) -> AssistantMessage {
    parse_assistant_input_items_with_api(API_NAME, items)
}

/// Replay a sequence of pre-decoded Codex stream events through the
/// provider's state machine and return the finalized
/// [`AssistantMessage`]. Each event runs through the §7.4.5
/// normalization layer first (legacy `response.done` /
/// `response.incomplete` rewrites, terminal `error` /
/// `response.failed` events surface as message-level errors), then the
/// shared §7.3.6 state machine consumes the normalized event under the
/// Codex API identifier and pricing curve.
///
/// Mirror of [`super::responses::replay_sse_events`] for round-trip
/// tests; the live provider uses the same machinery through
/// [`run_stream_inner`].
pub fn replay_sse_events(
    model: &ModelInfo,
    events: impl IntoIterator<Item = ResponseStreamEvent>,
    requested_tier: Option<ServiceTier>,
) -> AssistantMessage {
    let mut state = StreamState::new_with(API_NAME, model, requested_tier, CODEX_COST_MULTIPLIER);
    for ev in events {
        match normalize_codex_event(ev) {
            Ok(NormalizedEvent::Forward(ev)) => {
                let _ = state.process(ev);
            }
            Ok(NormalizedEvent::Terminal(ev)) => {
                let _ = state.process(ev);
                break;
            }
            Err(err) => {
                // Codex backends sometimes inject a terminal `error`
                // SSE frame mid-stream; surface it as a finalized
                // error message so the round-trip suite can assert on
                // the classified payload without spinning up a full
                // provider run.
                let mut error = AssistantMessage::empty();
                error.api = API_NAME.to_string();
                error.provider = model.provider.clone();
                error.model = model.id.clone();
                error.stop_reason = StopReason::Error;
                error.error = Some(err);
                return error;
            }
        }
    }
    match state.finalize() {
        AssistantMessageEvent::Done { message, .. }
        | AssistantMessageEvent::Error { error: message, .. } => message,
        other => panic!("StreamState::finalize returned non-terminal event: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Request body construction (§7.4.3)
// ---------------------------------------------------------------------------

fn build_request(
    model: &ModelInfo,
    context: &Context,
    options: &StreamOptions,
    reasoning: Option<&ThinkingLevel>,
) -> CreateResponseRequest {
    // §7.4.2: system prompt routed via the top-level `instructions`
    // field, not as a developer/system input item. We seed `input`
    // empty — `convert_messages` only appends user/assistant/tool-
    // result items.
    let mut input: Vec<ResponseInputItem> = Vec::new();
    let transformed = transform_messages(&context.messages, model);
    convert_messages(API_NAME, &transformed, &mut input);

    let tools: Vec<ResponseTool> = context.tools.iter().map(to_codex_tool).collect();

    // §7.4.3 reasoning configuration. Non-reasoning models reject the
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

    // §7.4.3: `prompt_cache_key` from session_id; `prompt_cache_retention`
    // never sent for Codex (the backend doesn't expose retention tuning).
    let prompt_cache_key = options.session_id.clone();

    let service_tier = options.service_tier.as_ref().map(map_service_tier);

    // §7.4.2: instructions carries the system prompt (or the §7.4.3
    // default if the caller didn't set one).
    let instructions = context
        .system_prompt
        .clone()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "You are a helpful assistant.".to_string());

    CreateResponseRequest {
        model: model.id.clone(),
        input: ResponseInput::Items(input),
        instructions: Some(ResponseInstructions::String(instructions)),
        tools,
        // §7.4.3: tool_choice always "auto".
        tool_choice: Some(ResponseToolChoice::String("auto".to_string())),
        // §7.4.3: parallel_tool_calls always true.
        parallel_tool_calls: Some(true),
        // §7.4.3: max_output_tokens omitted.
        max_output_tokens: None,
        temperature: options.temperature,
        reasoning: reasoning_cfg,
        // §7.4.3: text.verbosity omitted so the server default applies.
        text: None,
        stream: Some(true),
        // §7.4.3: store hardcoded false; server rejects true.
        store: Some(false),
        include,
        service_tier,
        prompt_cache_key,
        // §7.4.3: prompt_cache_retention never sent.
        prompt_cache_retention: None,
        ..Default::default()
    }
}

/// §7.4.3: Codex tools omit `strict` (the endpoint rejects requests
/// carrying it). Otherwise identical to the §7.3.2 shape.
fn to_codex_tool(tool: &ToolDefinition) -> ResponseTool {
    ResponseTool::Function {
        name: tool.name.clone(),
        description: Some(tool.description.clone()),
        parameters: Some(tool.parameters.clone()),
        strict: None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{InputModality, ModelCost};
    use crate::types::{
        AssistantContent, AssistantMessage as UnifiedAssistantMessage, CacheRetention,
        Message as UnifiedMessage, TextContent, ToolCall, UserContent, UserMessage,
    };
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use openai_sdk::types::common::ApiError;
    use openai_sdk::types::responses::MessagePhase;

    fn fake_model(id: &str, reasoning: bool) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            name: id.into(),
            api: API_NAME.into(),
            provider: "openai-codex".into(),
            base_url: DEFAULT_BASE_URL.into(),
            reasoning,
            supports_xhigh: false,
            supports_adaptive_thinking: false,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 1.0,
                output: 2.0,
                cache_read: 0.1,
                cache_write: 0.0,
            },
            context_window: 200_000,
            max_tokens: 16_000,
            headers: None,
        }
    }

    fn make_jwt(payload: &serde_json::Value) -> String {
        let header = URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\",\"typ\":\"JWT\"}");
        let body = URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(b"sig");
        format!("{header}.{body}.{sig}")
    }

    #[test]
    fn user_agent_starts_with_aj_prefix() {
        let ua = user_agent();
        assert!(
            ua.starts_with("aj/"),
            "expected User-Agent to start with `aj/`, got `{ua}`"
        );
    }

    #[test]
    fn build_request_routes_system_prompt_to_instructions() {
        let mut ctx = Context::new("you are a test");
        ctx.messages.push(UnifiedMessage::User(UserMessage {
            content: vec![UserContent::text("hi")],
            timestamp: 0,
        }));
        let req = build_request(
            &fake_model("gpt-5.1", false),
            &ctx,
            &StreamOptions::default(),
            None,
        );

        match &req.instructions {
            Some(ResponseInstructions::String(s)) => assert_eq!(s, "you are a test"),
            other => panic!("expected instructions=String, got {other:?}"),
        }
        // The system prompt must NOT be inlined as an input item.
        match &req.input {
            ResponseInput::Items(items) => {
                for item in items {
                    if let ResponseInputItem::Message { role, .. } = item {
                        assert!(
                            !matches!(
                                role,
                                openai_sdk::types::responses::InputRole::System
                                    | openai_sdk::types::responses::InputRole::Developer
                            ),
                            "system prompt leaked into input items: {item:?}"
                        );
                    }
                }
            }
            other => panic!("expected input=Items, got {other:?}"),
        }
    }

    #[test]
    fn build_request_uses_default_instructions_when_system_prompt_empty() {
        let ctx = Context::new("");
        let req = build_request(
            &fake_model("gpt-5.1", false),
            &ctx,
            &StreamOptions::default(),
            None,
        );
        match &req.instructions {
            Some(ResponseInstructions::String(s)) => assert_eq!(s, "You are a helpful assistant."),
            other => panic!("expected instructions default, got {other:?}"),
        }
    }

    #[test]
    fn build_request_hardcodes_store_false_and_tool_choice_auto() {
        let ctx = Context::new("hello");
        let req = build_request(
            &fake_model("gpt-5.1", false),
            &ctx,
            &StreamOptions::default(),
            None,
        );
        assert_eq!(req.store, Some(false));
        assert!(matches!(
            req.tool_choice,
            Some(ResponseToolChoice::String(ref s)) if s == "auto"
        ));
        assert_eq!(req.parallel_tool_calls, Some(true));
    }

    #[test]
    fn build_request_omits_strict_text_verbosity_max_output_tokens_and_retention() {
        let ctx = Context::new("hello");
        let req = build_request(
            &fake_model("gpt-5.1", false),
            &ctx,
            &StreamOptions {
                max_tokens: Some(1000),
                cache_retention: CacheRetention::Long,
                session_id: Some("sid".into()),
                ..Default::default()
            },
            None,
        );
        // text.verbosity omitted.
        assert!(req.text.is_none(), "text config must be omitted");
        // max_output_tokens omitted regardless of caller-provided cap.
        assert!(req.max_output_tokens.is_none());
        // prompt_cache_retention omitted regardless of cache_retention.
        assert!(req.prompt_cache_retention.is_none());
        // prompt_cache_key driven entirely by session_id.
        assert_eq!(req.prompt_cache_key.as_deref(), Some("sid"));
    }

    #[test]
    fn build_request_tool_omits_strict_field() {
        let mut ctx = Context::new("hello");
        ctx.tools.push(ToolDefinition {
            name: "ls".into(),
            description: "list directory".into(),
            parameters: serde_json::json!({"type":"object"}),
        });
        let req = build_request(
            &fake_model("gpt-5.1", false),
            &ctx,
            &StreamOptions::default(),
            None,
        );
        let serialized = serde_json::to_value(&req).expect("serialize");
        let tools = serialized
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools array");
        assert_eq!(tools.len(), 1);
        let tool_obj = &tools[0];
        assert!(
            tool_obj.get("strict").is_none(),
            "strict field must be omitted from Codex tool definitions, got {tool_obj}"
        );
    }

    #[test]
    fn build_request_sets_reasoning_only_on_reasoning_models() {
        let ctx = Context::new("hello");
        let req_non = build_request(
            &fake_model("gpt-5.1", false),
            &ctx,
            &StreamOptions::default(),
            Some(&ThinkingLevel::Medium),
        );
        assert!(req_non.reasoning.is_none());
        assert!(req_non.include.is_empty());

        let req_yes = build_request(
            &fake_model("gpt-5.1", true),
            &ctx,
            &StreamOptions::default(),
            Some(&ThinkingLevel::Medium),
        );
        let r = req_yes.reasoning.expect("reasoning set");
        assert!(matches!(r.effort, Some(ReasoningEffort::Medium)));
        assert_eq!(
            req_yes.include,
            vec![ResponseIncludable::ReasoningEncryptedContent]
        );
    }

    #[test]
    fn codex_cost_multiplier_default_curve() {
        assert!((codex_cost_multiplier("gpt-5.1", None, None) - 1.0).abs() < f64::EPSILON);
        assert!(
            (codex_cost_multiplier("gpt-5.1", Some(&OpenAIServiceTier::Flex), None) - 0.5).abs()
                < f64::EPSILON
        );
        assert!(
            (codex_cost_multiplier("gpt-5.1", Some(&OpenAIServiceTier::Priority), None) - 2.0)
                .abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn codex_cost_multiplier_gpt_5_5_priority_exception() {
        assert!(
            (codex_cost_multiplier("gpt-5.5", Some(&OpenAIServiceTier::Priority), None) - 2.5)
                .abs()
                < f64::EPSILON
        );
        // Flex isn't subject to the exception.
        assert!(
            (codex_cost_multiplier("gpt-5.5", Some(&OpenAIServiceTier::Flex), None) - 0.5).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn codex_cost_multiplier_falls_back_to_requested_when_server_silent() {
        assert!(
            (codex_cost_multiplier("gpt-5.1", None, Some(&OpenAIServiceTier::Flex)) - 0.5).abs()
                < f64::EPSILON
        );
    }

    #[test]
    fn friendly_message_for_usage_limit_with_plan_and_resets_at_envelope() {
        let now_secs = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        )
        .unwrap();
        let resets = now_secs + 17 * 60;
        let body = format!(
            r#"{{"error":{{"code":"usage_limit_reached","plan_type":"Pro","resets_at":{resets}}}}}"#
        );
        let friendly = friendly_codex_message(Some("usage_limit_reached"), None, 429, &body)
            .expect("friendly message");
        assert!(friendly.starts_with("You have hit your ChatGPT usage limit"));
        assert!(friendly.contains("(pro plan)"));
        // Allow for rounding to drop a minute either side of 17.
        assert!(
            friendly.contains("16 min") || friendly.contains("17 min"),
            "expected ~17-minute reset, got: {friendly}"
        );
    }

    #[test]
    fn friendly_message_for_429_without_specific_code() {
        let friendly = friendly_codex_message(None, None, 429, "{}").expect("friendly message");
        assert_eq!(friendly, "You have hit your ChatGPT usage limit.");
    }

    #[test]
    fn friendly_message_skipped_for_non_usage_errors() {
        assert!(friendly_codex_message(Some("invalid_request_error"), None, 400, "{}").is_none());
        assert!(friendly_codex_message(None, None, 500, "boom").is_none());
    }

    #[test]
    fn classify_codex_client_error_overlays_friendly_message_on_429() {
        let err = ClientError::ApiError {
            error: ApiError {
                message: r#"{"error":{"code":"usage_limit_reached"}}"#.into(),
                r#type: None,
                param: None,
                code: Some("usage_limit_reached".into()),
            },
            http_status: 429,
            retry_after: None,
        };
        let classified = classify_codex_client_error(&err);
        assert_eq!(classified.category, ErrorCategory::RateLimit);
        assert!(
            classified
                .message
                .starts_with("You have hit your ChatGPT usage limit")
        );
    }

    #[test]
    fn normalize_codex_event_rewrites_legacy_response_done_to_completed() {
        let raw = serde_json::json!({
            "type": "response.done",
            "response": {
                "id": "resp_1",
                "object": "response",
                "created_at": 0.0,
                "model": "gpt-5.1",
                "output": [],
                "parallel_tool_calls": true,
                "tools": [],
                "status": "completed",
            },
            "sequence_number": 1,
        });
        let event = serde_json::from_value::<ResponseStreamEvent>(raw).expect("parse Other");
        let normalized = normalize_codex_event(event).expect("ok");
        match normalized {
            NormalizedEvent::Terminal(ResponseStreamEvent::ResponseCompleted {
                response, ..
            }) => {
                assert_eq!(response.id, "resp_1");
                assert_eq!(response.status, Some(ResponseStatus::Completed));
            }
            other => panic!("expected Terminal(ResponseCompleted), got {other:?}"),
        }
    }

    #[test]
    fn normalize_codex_event_rewrites_legacy_response_incomplete_preserving_status() {
        let raw = serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "id": "resp_2",
                "object": "response",
                "created_at": 0.0,
                "model": "gpt-5.1",
                "output": [],
                "parallel_tool_calls": true,
                "tools": [],
            },
            "sequence_number": 1,
        });
        let event = serde_json::from_value::<ResponseStreamEvent>(raw).expect("parse Other");
        let normalized = normalize_codex_event(event).expect("ok");
        match normalized {
            NormalizedEvent::Terminal(ResponseStreamEvent::ResponseCompleted {
                response, ..
            }) => {
                assert_eq!(response.status, Some(ResponseStatus::Incomplete));
            }
            other => panic!("expected Terminal(ResponseCompleted), got {other:?}"),
        }
    }

    #[test]
    fn normalize_codex_event_surfaces_top_level_error_as_assistant_error() {
        let event = ResponseStreamEvent::Error {
            code: Some("rate_limit_exceeded".into()),
            message: "slow down".into(),
            sequence_number: 1,
        };
        let result = normalize_codex_event(event);
        let err = result.err().expect("Err");
        assert_eq!(err.category, ErrorCategory::RateLimit);
        assert!(err.message.contains("slow down"));
    }

    #[test]
    fn normalize_codex_event_forwards_unknown_other_events() {
        let value = serde_json::json!({"type": "response.unknown_event", "sequence_number": 1});
        let event = ResponseStreamEvent::Other(value.clone());
        let normalized = normalize_codex_event(event).expect("ok");
        match normalized {
            NormalizedEvent::Forward(ResponseStreamEvent::Other(v)) => assert_eq!(v, value),
            other => panic!("expected Forward(Other), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn run_stream_emits_auth_error_when_api_key_missing() {
        let model = fake_model("gpt-5.1", false);
        let ctx = Context::new("hi");
        let options = StreamOptions::default();
        let mut stream = OpenAiCodexResponsesProvider.stream(&model, &ctx, &options);
        while stream.next().await.is_some() {}
        let final_message = stream.result().await;
        assert_eq!(final_message.stop_reason, StopReason::Error);
        let err = final_message.error.expect("error populated");
        assert_eq!(err.category, ErrorCategory::Auth);
        assert!(err.message.contains("OAuth access token"));
    }

    #[tokio::test]
    async fn run_stream_emits_auth_error_when_jwt_lacks_account_id() {
        let model = fake_model("gpt-5.1", false);
        let ctx = Context::new("hi");
        // Valid-looking JWT but no account id claim.
        let token = make_jwt(&serde_json::json!({"sub": "missing-claim"}));
        let options = StreamOptions {
            api_key: Some(token),
            ..Default::default()
        };
        let mut stream = OpenAiCodexResponsesProvider.stream(&model, &ctx, &options);
        while stream.next().await.is_some() {}
        let final_message = stream.result().await;
        assert_eq!(final_message.stop_reason, StopReason::Error);
        let err = final_message.error.expect("error populated");
        assert_eq!(err.category, ErrorCategory::Auth);
        assert!(err.message.contains("chatgpt_account_id"));
    }

    #[test]
    fn to_codex_tool_emits_no_strict_field() {
        let tool = ToolDefinition {
            name: "x".into(),
            description: "d".into(),
            parameters: serde_json::json!({}),
        };
        let codex_tool = to_codex_tool(&tool);
        match codex_tool {
            ResponseTool::Function { strict, .. } => assert!(strict.is_none()),
            _ => panic!("expected ResponseTool::Function"),
        }
    }

    // Suppress unused-warning fallout from optional dependencies.
    #[test]
    fn assistant_message_field_accessors_compile() {
        let mut m = UnifiedAssistantMessage::empty();
        m.api = API_NAME.into();
        m.content.push(AssistantContent::Text(TextContent {
            text: "hi".into(),
            text_signature: None,
        }));
        m.content.push(AssistantContent::ToolCall(ToolCall {
            id: "call|item".into(),
            name: "x".into(),
            arguments: Value::Null,
        }));
        assert_eq!(m.content.len(), 2);
        // MessagePhase / ApiErrorResponse imports kept alive.
        let _phase: Option<MessagePhase> = None;
    }
}
