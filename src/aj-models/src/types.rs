//! Provider-independent unified message types.
//!
//! All provider implementations produce and consume these types. They are the
//! canonical representation for conversations, tool calls, and streaming
//! options across Anthropic, OpenAI, and any future providers.
//!
//! See `docs/models-spec.md` §1 and §4 for the full design.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// §1.1 Content Types
// ---------------------------------------------------------------------------

/// Text content block.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TextContent {
    pub text: String,
    /// Opaque provider-specific signature for multi-turn replay.
    /// Anthropic: unused. OpenAI Responses: JSON-encoded TextSignatureV1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text_signature: Option<String>,
}

/// Extended thinking / reasoning content.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ThinkingContent {
    pub thinking: String,
    /// Opaque signature for multi-turn replay. Anthropic: base64 signature,
    /// OpenAI: reasoning item JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_signature: Option<String>,
    /// When true, content was redacted by safety filters. The encrypted
    /// payload is in `thinking_signature` for multi-turn continuity.
    #[serde(default)]
    pub redacted: bool,
}

/// Base64-encoded image content.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ImageContent {
    /// Base64-encoded image data.
    pub data: String,
    /// MIME type, e.g. "image/png".
    pub mime_type: String,
}

/// A tool invocation requested by the model.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolCall {
    /// Provider-assigned tool call ID, used to match results.
    pub id: String,
    /// Tool name (must match a `ToolDefinition.name`).
    pub name: String,
    /// Parsed JSON arguments for the tool.
    pub arguments: Value,
}

/// Content that can appear in an assistant message.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum AssistantContent {
    #[serde(rename = "text")]
    Text(TextContent),
    #[serde(rename = "thinking")]
    Thinking(ThinkingContent),
    #[serde(rename = "tool_call")]
    ToolCall(ToolCall),
}

/// Content that can appear in a user message.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type")]
pub enum UserContent {
    #[serde(rename = "text")]
    Text(TextContent),
    #[serde(rename = "image")]
    Image(ImageContent),
}

// ---------------------------------------------------------------------------
// §1.2 Messages
// ---------------------------------------------------------------------------

/// A message from the user.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UserMessage {
    pub content: Vec<UserContent>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A message from the assistant (model response).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    /// Which API produced this message (e.g. "anthropic-messages",
    /// "openai-completions").
    pub api: String,
    /// Which provider (e.g. "anthropic", "openai").
    pub provider: String,
    /// Exact model ID used.
    pub model: String,
    /// Provider-specific response/message ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    /// Failure detail when `stop_reason` is `Error` or `Aborted`.
    /// Populated by providers per `docs/models-spec.md` §10.3.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AssistantError>,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A tool result returned to the model.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolResultMessage {
    /// ID of the tool call this result corresponds to.
    pub tool_call_id: String,
    /// Name of the tool that was called.
    pub tool_name: String,
    /// Result content — text and/or images.
    pub content: Vec<UserContent>,
    /// Optional structured details preserved for UI/logs but never
    /// sent to the provider. Tools attach rich metadata (diffs, file
    /// paths, exit codes, …) here for display without forcing it
    /// through the model. Serialized with the thread; provider
    /// message conversion ignores it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
    /// Whether the tool execution resulted in an error.
    #[serde(default)]
    pub is_error: bool,
    /// Unix timestamp in milliseconds.
    pub timestamp: i64,
}

/// A conversation message — one of user, assistant, or tool result.
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "role")]
pub enum Message {
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "tool_result")]
    ToolResult(ToolResultMessage),
}

// ---------------------------------------------------------------------------
// §1.3 Stop Reason, Usage & Error
// ---------------------------------------------------------------------------

/// Why the model stopped generating.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// Normal completion (end of turn).
    Stop,
    /// Hit the maximum token limit.
    Length,
    /// Model requested tool use.
    ToolUse,
    /// An error occurred during generation.
    Error,
    /// Client-synthesized: the request was cancelled locally (e.g. the
    /// stream was dropped). No provider ever returns this directly.
    Aborted,
}

impl Default for StopReason {
    fn default() -> Self {
        Self::Stop
    }
}

/// Token usage for a single model response.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub cost: UsageCost,
}

/// Dollar costs broken down by token category.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct UsageCost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

/// Carried on `AssistantMessage.error` when `stop_reason == Error`
/// or `Aborted`.
///
/// Providers classify upstream failures into one of the [`ErrorCategory`]
/// values so callers can decide retry behaviour without regex-matching
/// the message string. Per-provider classification tables live in
/// `docs/models-spec.md` §10.3; retry semantics in §10.4.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AssistantError {
    pub category: ErrorCategory,
    /// Human-readable failure message. Whatever the upstream surfaced,
    /// cleaned up (e.g. JSON-decoded `error.message`).
    pub message: String,
    /// Server-requested retry delay in milliseconds, populated from
    /// the `Retry-After` header or a body hint when present. `None`
    /// when the provider didn't specify a delay. Only meaningful for
    /// `RateLimit`, `Overloaded`, and `Transient` categories.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// HTTP status from the originating response; `None` for
    /// transport-level failures, stream drops, and client aborts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

/// Classification of a failure terminating an assistant turn.
///
/// Categories are stable and form the contract callers key retry
/// behaviour off. See `docs/models-spec.md` §10.2 for the retryable /
/// not-retryable split and §10.3 for per-provider mapping tables.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    /// 401 / 403 or OAuth refresh failure. Not retryable without
    /// re-authenticating.
    Auth,
    /// 429 rate-limit response. Retryable; honour `retry_after_ms`.
    RateLimit,
    /// Provider-overload response (Anthropic 529, OpenAI 503 overload
    /// body). Retryable with backoff.
    Overloaded,
    /// 5xx, transport error, or stream drop mid-response. Retryable,
    /// but note that partial output may already have been emitted.
    Transient,
    /// 400 whose message matches the context-overflow patterns.
    /// Not retryable without reducing context.
    ContextOverflow,
    /// 400 that is not a context overflow (malformed request, unknown
    /// parameter, quota / billing, etc.). Not retryable.
    InvalidRequest,
    /// Safety filter refusal (Anthropic `refusal`, OpenAI
    /// `content_filter`, Responses `response.refusal`). Not retryable.
    ContentFilter,
    /// Client dropped the stream / cancelled the request.
    /// Pairs with [`StopReason::Aborted`].
    Aborted,
    /// Catchall when the provider can't map the failure onto one of
    /// the above. Treat as not retryable by default.
    Unknown,
}

impl ErrorCategory {
    /// Whether errors in this category are safe to automatically
    /// retry with backoff. See `docs/models-spec.md` §10.2.
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::RateLimit | Self::Overloaded | Self::Transient)
    }
}

impl AssistantError {
    /// Convenience constructor for a category-only error with no
    /// HTTP context. Useful for transport failures and synthesized
    /// errors before any HTTP response is in hand.
    pub fn new(category: ErrorCategory, message: impl Into<String>) -> Self {
        Self {
            category,
            message: message.into(),
            retry_after_ms: None,
            http_status: None,
        }
    }
}

// ---------------------------------------------------------------------------
// §1.4 Tool Definition
// ---------------------------------------------------------------------------

/// Description of a tool the model can invoke.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    /// JSON Schema describing the tool's parameters.
    pub parameters: Value,
}

// ---------------------------------------------------------------------------
// §1.5 Context (input to a streaming call)
// ---------------------------------------------------------------------------

/// Everything the provider needs to make a streaming inference call.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

// ---------------------------------------------------------------------------
// §1.6 Thinking Level
// ---------------------------------------------------------------------------

/// Controls the depth of extended thinking / reasoning.
///
/// Serialized in lower-case form: `"minimal"`, `"low"`, `"medium"`,
/// `"high"`, `"xhigh"`. The wire value for [`Self::XHigh`] matches
/// OpenAI's `reasoning_effort: "xhigh"` exactly.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    /// Maximum reasoning effort. Maps to Anthropic adaptive
    /// `output_config: {effort: "max"}` (Opus 4.6 only) and OpenAI
    /// `reasoning_effort: "xhigh"` (GPT-5.2+ only). For models that
    /// don't support this level, providers fall back to
    /// [`Self::High`].
    XHigh,
}

// ---------------------------------------------------------------------------
// §4 Stream Options
// ---------------------------------------------------------------------------

/// Prompt cache retention preference.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub enum CacheRetention {
    None,
    #[default]
    Short,
    Long,
}

/// Service tier override for OpenAI Responses requests. Ignored by
/// non-Responses providers. See `docs/models-spec.md` §7.3 for cost
/// multipliers.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    /// 0.5× cost; best-effort latency.
    Flex,
    /// 2× cost; prioritized capacity.
    Priority,
}

/// Reasoning summary verbosity for OpenAI Responses requests.
/// Ignored by non-Responses providers. Defaults to [`Self::Auto`]
/// when reasoning is enabled. See `docs/models-spec.md` §7.3.2.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningSummary {
    /// Provider chooses summary verbosity.
    Auto,
    /// More verbose reasoning summaries.
    Detailed,
    /// Shorter reasoning summaries.
    Concise,
}

/// Controls whether the model must, may, or must not use tools.
///
/// When [`StreamOptions::tool_choice`] is `None`, providers apply
/// their own default (typically [`Self::Auto`]).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoice {
    /// Model decides whether to call a tool (default behavior).
    Auto,
    /// Model must not call any tools.
    None,
    /// Model must call at least one tool (any tool).
    Required,
    /// Model must call the specific named tool.
    Tool { name: String },
}

/// Callback invoked with the raw outgoing request body just before
/// it's sent. Useful for logging, recording test fixtures, or tracing
/// provider-specific payload shape. Must not mutate the body —
/// providers treat it as read-only.
///
/// Wrapped in a newtype so [`StreamOptions`] can keep its derived
/// [`Debug`] / [`Clone`] impls and so the field can be skipped from
/// serde without breaking the rest of the struct's wire shape.
#[derive(Clone)]
pub struct OnPayload(pub Arc<dyn Fn(&Value) + Send + Sync>);

impl OnPayload {
    /// Wrap a closure as an [`OnPayload`] callback.
    pub fn new<F>(f: F) -> Self
    where
        F: Fn(&Value) + Send + Sync + 'static,
    {
        Self(Arc::new(f))
    }

    /// Invoke the callback with the outgoing request body.
    pub fn call(&self, body: &Value) {
        (self.0)(body)
    }
}

impl std::fmt::Debug for OnPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnPayload").finish_non_exhaustive()
    }
}

/// Resolver for the provider API key.
///
/// Providers call [`StreamOptions::resolve_api_key`] before each
/// streaming HTTP request to obtain the credential they should pass
/// to the upstream API. The default resolver returns the static
/// [`StreamOptions::api_key`] value; callers that need to refresh
/// an OAuth token *between* inferences (e.g. a long-running tool
/// outlived the access-token expiry) replace it via
/// [`StreamOptions::set_api_key_resolver`] with a closure that
/// reads from their auth store on every call.
///
/// The closure returns a boxed future so the resolver can perform
/// asynchronous work (an HTTP refresh, a disk read with a file
/// lock). Wrapped in an [`Arc`] so cloning the [`StreamOptions`]
/// (the agent does this per inference) only bumps a refcount.
#[derive(Clone)]
pub struct ApiKeyResolver(
    pub  Arc<
        dyn Fn() -> std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<String, String>> + Send>,
            > + Send
            + Sync,
    >,
);

impl ApiKeyResolver {
    /// Wrap an async closure as an [`ApiKeyResolver`].
    pub fn new<F, Fut>(f: F) -> Self
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<String, String>> + Send + 'static,
    {
        Self(Arc::new(move || Box::pin(f())))
    }

    /// Invoke the resolver. Returns the resolved key or an error
    /// message the provider surfaces as an [`Auth`]-category
    /// [`crate::errors::AssistantError`].
    pub async fn call(&self) -> Result<String, String> {
        (self.0)().await
    }
}

impl std::fmt::Debug for ApiKeyResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ApiKeyResolver").finish_non_exhaustive()
    }
}

/// Options passed to any streaming call.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct StreamOptions {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// Static API key. Used when [`StreamOptions::api_key_resolver`]
    /// is `None`. Callers that want per-call resolution (OAuth
    /// refresh, dynamic credential rotation) should install a
    /// resolver via [`StreamOptions::set_api_key_resolver`] instead
    /// of mutating this field. Providers go through
    /// [`StreamOptions::resolve_api_key`] which prefers the resolver
    /// over the static value when both are set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Per-call API key resolver. Skipped in serde — closures aren't
    /// serializable, and persisting them across runs would be
    /// meaningless. When set, providers call this on every inference
    /// instead of reading [`StreamOptions::api_key`]; see
    /// [`StreamOptions::resolve_api_key`].
    #[serde(skip)]
    pub api_key_resolver: Option<ApiKeyResolver>,
    /// Prompt cache retention preference.
    pub cache_retention: CacheRetention,
    /// Session ID for providers that support session-based caching.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Extra HTTP headers merged with provider defaults.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// Metadata fields (e.g. Anthropic user_id for rate limiting).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<HashMap<String, Value>>,
    /// Optional debug callback invoked with the outgoing request body
    /// just before it's sent. Skipped in serde — callbacks aren't
    /// serializable, and persisting them would be meaningless.
    #[serde(skip)]
    pub on_payload: Option<OnPayload>,
    /// Responses-only: request a non-default service tier. Ignored by
    /// non-Responses providers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<ServiceTier>,
    /// Responses-only: reasoning summary verbosity. Ignored by
    /// non-Responses providers. Defaults to [`ReasoningSummary::Auto`]
    /// when reasoning is enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_summary: Option<ReasoningSummary>,
    /// Controls whether/how the model uses tools. When `None`, the
    /// provider default applies (typically [`ToolChoice::Auto`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<ToolChoice>,
}

/// Higher-level options that include reasoning control.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SimpleStreamOptions {
    #[serde(flatten)]
    pub base: StreamOptions,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ThinkingLevel>,
}

impl StreamOptions {
    /// Install a per-call API key resolver. Replaces any previous
    /// resolver; passing `None` clears it so providers fall back to
    /// the static [`StreamOptions::api_key`] value on the next
    /// inference. See [`ApiKeyResolver`] for the contract.
    pub fn set_api_key_resolver(&mut self, resolver: Option<ApiKeyResolver>) {
        self.api_key_resolver = resolver;
    }

    /// Resolve the API key the provider should use for this request.
    ///
    /// Order of preference:
    /// 1. [`StreamOptions::api_key_resolver`] if set — invoked
    ///    every call so OAuth refresh-on-demand works.
    /// 2. [`StreamOptions::api_key`] if set — used as the static
    ///    fallback.
    /// 3. `Err` if neither is set; the provider surfaces this as an
    ///    [`crate::errors::ErrorCategory::Auth`] failure.
    pub async fn resolve_api_key(&self) -> Result<String, String> {
        if let Some(resolver) = &self.api_key_resolver {
            return resolver.call().await;
        }
        self.api_key
            .clone()
            .ok_or_else(|| "missing api_key and no api_key_resolver installed".to_string())
    }
}

// ---------------------------------------------------------------------------
// Convenience constructors
// ---------------------------------------------------------------------------

impl TextContent {
    /// Create a plain text content block with no signature.
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            text_signature: None,
        }
    }
}

impl UserContent {
    /// Create a text user content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent::new(text))
    }

    /// Create an image user content block.
    pub fn image(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self::Image(ImageContent {
            data: data.into(),
            mime_type: mime_type.into(),
        })
    }
}

impl AssistantContent {
    /// Create a text assistant content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text(TextContent::new(text))
    }
}

impl UserMessage {
    /// Create a user message with a single text content block.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![UserContent::text(text)],
            timestamp: 0,
        }
    }
}

impl ToolResultMessage {
    /// Create a text-only tool result with no structured details.
    pub fn text(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        text: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            content: vec![UserContent::text(text)],
            details: None,
            is_error,
            timestamp: 0,
        }
    }
}

impl AssistantMessage {
    /// Create a default/empty assistant message (useful as a partial during
    /// streaming).
    pub fn empty() -> Self {
        Self {
            content: Vec::new(),
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::default(),
            error: None,
            timestamp: 0,
        }
    }
}

impl Context {
    /// Create a context with a system prompt and no prior messages or tools.
    pub fn new(system_prompt: impl Into<String>) -> Self {
        Self {
            system_prompt: Some(system_prompt.into()),
            messages: Vec::new(),
            tools: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_roundtrip() {
        // Verify that our Message enum serializes/deserializes correctly.
        let msg = Message::User(UserMessage::text("hello"));
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        match back {
            Message::User(u) => {
                assert_eq!(u.content.len(), 1);
                match &u.content[0] {
                    UserContent::Text(t) => assert_eq!(t.text, "hello"),
                    _ => panic!("expected text content"),
                }
            }
            _ => panic!("expected User message"),
        }
    }

    #[test]
    fn test_assistant_message_roundtrip() {
        let msg = Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContent::text("some text"),
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "let me think".into(),
                    thinking_signature: Some("sig123".into()),
                    redacted: false,
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    arguments: serde_json::json!({"path": "/tmp/test"}),
                }),
            ],
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            model: "claude-sonnet-4-20250514".into(),
            response_id: Some("resp_123".into()),
            usage: Usage {
                input: 100,
                output: 50,
                cache_read: 10,
                cache_write: 5,
                total_tokens: 165,
                cost: UsageCost::default(),
            },
            stop_reason: StopReason::ToolUse,
            error: None,
            timestamp: 1234567890,
        });

        let json = serde_json::to_string_pretty(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        match back {
            Message::Assistant(a) => {
                assert_eq!(a.content.len(), 3);
                assert_eq!(a.stop_reason, StopReason::ToolUse);
                assert_eq!(a.usage.input, 100);
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[test]
    fn test_tool_result_roundtrip() {
        let msg = Message::ToolResult(ToolResultMessage::text(
            "call_1",
            "read_file",
            "file contents here",
            false,
        ));

        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        match back {
            Message::ToolResult(tr) => {
                assert_eq!(tr.tool_call_id, "call_1");
                assert_eq!(tr.tool_name, "read_file");
                assert!(!tr.is_error);
            }
            _ => panic!("expected ToolResult message"),
        }
    }

    #[test]
    fn test_stop_reason_default() {
        assert_eq!(StopReason::default(), StopReason::Stop);
    }

    #[test]
    fn test_cache_retention_default() {
        assert_eq!(CacheRetention::default(), CacheRetention::Short);
    }

    #[test]
    fn test_stream_options_default() {
        let opts = StreamOptions::default();
        assert_eq!(opts.cache_retention, CacheRetention::Short);
        assert!(opts.temperature.is_none());
        assert!(opts.api_key.is_none());
    }

    #[test]
    fn test_simple_stream_options_flatten() {
        // Verify that SimpleStreamOptions flattens base fields correctly
        // and that ThinkingLevel uses the spec'd lower-case wire form.
        let opts = SimpleStreamOptions {
            base: StreamOptions {
                temperature: Some(0.7),
                ..Default::default()
            },
            reasoning: Some(ThinkingLevel::High),
        };
        let json = serde_json::to_value(&opts).unwrap();
        // temperature should be at the top level due to #[serde(flatten)]
        assert_eq!(json["temperature"], 0.7);
        assert_eq!(json["reasoning"], "high");
    }

    #[test]
    fn test_thinking_level_xhigh_serde() {
        // §1.6 wire form: lower-case, single token ("xhigh", not "x-high").
        let json = serde_json::to_value(ThinkingLevel::XHigh).unwrap();
        assert_eq!(json, "xhigh");
        let back: ThinkingLevel = serde_json::from_str("\"xhigh\"").unwrap();
        assert_eq!(back, ThinkingLevel::XHigh);
        // Spot-check the other variants too.
        assert_eq!(
            serde_json::to_value(ThinkingLevel::Minimal).unwrap(),
            "minimal"
        );
        assert_eq!(
            serde_json::to_value(ThinkingLevel::Medium).unwrap(),
            "medium"
        );
    }

    #[test]
    fn test_tool_result_details_roundtrip() {
        // `details` is preserved through serialization but defaults to
        // None and is omitted from the wire when absent.
        let mut msg = ToolResultMessage::text("call_1", "bash", "hi", false);
        msg.details = Some(serde_json::json!({
            "kind": "Bash",
            "exit_code": 0,
        }));
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"details\""));
        let back: ToolResultMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.details.as_ref().unwrap()["exit_code"], 0);

        // Default constructor leaves details None and skips the field.
        let bare = ToolResultMessage::text("call_2", "ls", "out", false);
        let bare_json = serde_json::to_string(&bare).unwrap();
        assert!(!bare_json.contains("\"details\""));
        let back: ToolResultMessage = serde_json::from_str(&bare_json).unwrap();
        assert!(back.details.is_none());
    }

    #[test]
    fn test_tool_choice_roundtrip() {
        // Unit variants serialize as snake_case strings; the named
        // `Tool` variant carries its struct payload.
        assert_eq!(
            serde_json::to_value(ToolChoice::Auto).unwrap(),
            serde_json::json!("auto")
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::None).unwrap(),
            serde_json::json!("none")
        );
        assert_eq!(
            serde_json::to_value(ToolChoice::Required).unwrap(),
            serde_json::json!("required")
        );
        let named = ToolChoice::Tool {
            name: "read_file".into(),
        };
        let json = serde_json::to_value(&named).unwrap();
        assert_eq!(json, serde_json::json!({"tool": {"name": "read_file"}}));
        let back: ToolChoice = serde_json::from_value(json).unwrap();
        assert_eq!(back, named);
    }

    #[test]
    fn test_service_tier_and_reasoning_summary_roundtrip() {
        assert_eq!(serde_json::to_value(ServiceTier::Flex).unwrap(), "flex");
        assert_eq!(
            serde_json::to_value(ServiceTier::Priority).unwrap(),
            "priority"
        );
        assert_eq!(
            serde_json::to_value(ReasoningSummary::Auto).unwrap(),
            "auto"
        );
        assert_eq!(
            serde_json::to_value(ReasoningSummary::Detailed).unwrap(),
            "detailed"
        );
        assert_eq!(
            serde_json::to_value(ReasoningSummary::Concise).unwrap(),
            "concise"
        );
    }

    #[test]
    fn test_stream_options_new_fields() {
        // The new fields on StreamOptions default to None and are
        // skipped from the wire when absent.
        let opts = StreamOptions::default();
        assert!(opts.service_tier.is_none());
        assert!(opts.reasoning_summary.is_none());
        assert!(opts.tool_choice.is_none());
        assert!(opts.on_payload.is_none());
        let json = serde_json::to_value(&opts).unwrap();
        assert!(json.get("service_tier").is_none());
        assert!(json.get("reasoning_summary").is_none());
        assert!(json.get("tool_choice").is_none());
        // on_payload is #[serde(skip)] — never appears regardless of value.
        assert!(json.get("on_payload").is_none());

        // When set, they round-trip through serde (modulo on_payload
        // which is intentionally not serialized).
        let opts = StreamOptions {
            service_tier: Some(ServiceTier::Flex),
            reasoning_summary: Some(ReasoningSummary::Concise),
            tool_choice: Some(ToolChoice::Required),
            ..Default::default()
        };
        let json = serde_json::to_string(&opts).unwrap();
        let back: StreamOptions = serde_json::from_str(&json).unwrap();
        assert_eq!(back.service_tier, Some(ServiceTier::Flex));
        assert_eq!(back.reasoning_summary, Some(ReasoningSummary::Concise));
        assert_eq!(back.tool_choice, Some(ToolChoice::Required));
    }

    #[test]
    fn test_on_payload_skipped_in_serde_but_invokable() {
        use std::sync::Mutex;
        // The callback is invokable through the wrapper and is skipped
        // by serde so its presence doesn't break round-trip.
        let captured: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_cb = Arc::clone(&captured);
        let cb = OnPayload::new(move |body: &Value| {
            captured_cb.lock().unwrap().push(body.clone());
        });
        let opts = StreamOptions {
            on_payload: Some(cb),
            ..Default::default()
        };
        // Round-trip drops the callback — that's the whole point of
        // #[serde(skip)] — but should not error.
        let json = serde_json::to_string(&opts).unwrap();
        let back: StreamOptions = serde_json::from_str(&json).unwrap();
        assert!(back.on_payload.is_none());

        // And invoking the callback through the original wrapper works.
        opts.on_payload
            .as_ref()
            .unwrap()
            .call(&serde_json::json!({"hello": "world"}));
        let log = captured.lock().unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0]["hello"], "world");

        // OnPayload's Debug impl doesn't try to format the closure.
        let dbg = format!("{:?}", opts);
        assert!(dbg.contains("OnPayload"));
    }

    #[test]
    fn test_context_constructor() {
        let ctx = Context::new("You are a helpful assistant.");
        assert_eq!(
            ctx.system_prompt.as_deref(),
            Some("You are a helpful assistant.")
        );
        assert!(ctx.messages.is_empty());
        assert!(ctx.tools.is_empty());
    }

    #[tokio::test]
    async fn resolve_api_key_prefers_resolver_when_set() {
        let opts = StreamOptions {
            api_key: Some("static-key".to_string()),
            api_key_resolver: Some(ApiKeyResolver::new(|| async {
                Ok("resolved-key".to_string())
            })),
            ..Default::default()
        };
        let resolved = opts.resolve_api_key().await.unwrap();
        assert_eq!(resolved, "resolved-key");
    }

    #[tokio::test]
    async fn resolve_api_key_falls_back_to_static_when_no_resolver() {
        let opts = StreamOptions {
            api_key: Some("static-key".to_string()),
            ..Default::default()
        };
        let resolved = opts.resolve_api_key().await.unwrap();
        assert_eq!(resolved, "static-key");
    }

    #[tokio::test]
    async fn resolve_api_key_errors_when_neither_is_set() {
        let opts = StreamOptions::default();
        let err = opts.resolve_api_key().await.unwrap_err();
        assert!(err.contains("missing api_key"), "got {err:?}");
    }

    #[tokio::test]
    async fn resolve_api_key_surfaces_resolver_error_verbatim() {
        let opts = StreamOptions {
            api_key_resolver: Some(ApiKeyResolver::new(|| async {
                Err("token refresh failed: 401".to_string())
            })),
            ..Default::default()
        };
        let err = opts.resolve_api_key().await.unwrap_err();
        assert_eq!(err, "token refresh failed: 401");
    }

    #[tokio::test]
    async fn api_key_resolver_is_invoked_per_call() {
        // A resolver that increments a shared counter on every call
        // proves the provider goes through the resolver on each
        // inference rather than caching the first value.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let counter: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        let opts = StreamOptions {
            api_key_resolver: Some(ApiKeyResolver::new(move || {
                let counter = Arc::clone(&counter_clone);
                async move {
                    let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    Ok(format!("call-{n}"))
                }
            })),
            ..Default::default()
        };

        let first = opts.resolve_api_key().await.unwrap();
        let second = opts.resolve_api_key().await.unwrap();
        let third = opts.resolve_api_key().await.unwrap();
        assert_eq!(first, "call-1");
        assert_eq!(second, "call-2");
        assert_eq!(third, "call-3");
        assert_eq!(counter.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn api_key_resolver_field_is_skipped_in_serde() {
        let opts = StreamOptions {
            api_key: Some("k".to_string()),
            api_key_resolver: Some(ApiKeyResolver::new(|| async { Ok("ignored".into()) })),
            ..Default::default()
        };
        let json = serde_json::to_string(&opts).unwrap();
        // The resolver field is skipped entirely; the static key
        // survives so a serialized->deserialized round-trip keeps
        // the auth path working off the static fallback.
        assert!(!json.contains("api_key_resolver"));
        let back: StreamOptions = serde_json::from_str(&json).unwrap();
        assert!(back.api_key_resolver.is_none());
        assert_eq!(back.api_key.as_deref(), Some("k"));
    }
}
