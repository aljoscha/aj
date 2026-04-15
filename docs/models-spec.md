# aj-models & Provider SDK Spec

This document specifies the target design for `aj-models`, `anthropic-sdk`, and `openai-sdk`.
It covers the unified message types, streaming protocol, provider implementations,
model registry, authentication (including OAuth), and cost tracking.

## Scope

- **Anthropic Messages API** (direct API key and OAuth/Claude Pro)
- **OpenAI Chat Completions API** (direct API key and OAuth/ChatGPT Plus)
- Provider-agnostic types that enable cross-provider conversation replay
- OAuth flows for both Anthropic and OpenAI

---

## 1. Unified Message Types (`aj-models::types`)

All providers produce and consume these types. They are provider-independent.

### 1.1 Content Types

```rust
/// Text content block.
struct TextContent {
    text: String,
    /// Opaque signature carrying message metadata required for
    /// multi-turn replay against APIs that pair output messages with
    /// server-side item IDs. Populated by `openai-responses` as a
    /// JSON-encoded `TextSignatureV1` (see §7.3.4). Ignored by
    /// `anthropic-messages` and `openai-completions`.
    text_signature: Option<String>,
}

/// Extended thinking / reasoning content.
struct ThinkingContent {
    thinking: String,
    /// Opaque signature for multi-turn replay.
    /// Populated by `anthropic-messages` (base64 signature from
    /// `signature_delta`) and by `openai-responses` (reasoning item id).
    /// Ignored by `openai-completions`.
    thinking_signature: Option<String>,
    /// When true, content was redacted by safety filters. The encrypted
    /// payload is in `thinking_signature` for multi-turn continuity.
    redacted: bool,
}

/// Base64-encoded image content.
struct ImageContent {
    data: String,       // base64
    mime_type: String,  // e.g. "image/png"
}

/// A tool invocation requested by the model.
struct ToolCall {
    id: String,
    name: String,
    arguments: serde_json::Value,
}

/// Union of content that can appear in an assistant message.
enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

/// Union of content in a user message.
enum UserContent {
    Text(TextContent),
    Image(ImageContent),
}
```

### 1.2 Messages

```rust
struct UserMessage {
    content: Vec<UserContent>,
    timestamp: i64,             // unix ms
}

struct AssistantMessage {
    content: Vec<AssistantContent>,
    /// Which API produced this message (e.g. "anthropic-messages", "openai-completions").
    api: String,
    /// Which provider (e.g. "anthropic", "openai").
    provider: String,
    /// Exact model ID used.
    model: String,
    /// Provider-specific response/message ID.
    response_id: Option<String>,
    usage: Usage,
    stop_reason: StopReason,
    error_message: Option<String>,
    timestamp: i64,
}

struct ToolResultMessage {
    tool_call_id: String,
    tool_name: String,
    content: Vec<UserContent>,  // text and/or images
    is_error: bool,
    timestamp: i64,
}

enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}
```

### 1.3 Stop Reason & Usage

```rust
enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    /// Client-synthesized: the request was cancelled locally (e.g. the
    /// stream was dropped). No provider ever returns this directly.
    Aborted,
}

struct Usage {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
    total_tokens: u64,
    cost: UsageCost,
}

struct UsageCost {
    input: f64,
    output: f64,
    cache_read: f64,
    cache_write: f64,
    total: f64,
}
```

### 1.4 Tool Definition

```rust
struct ToolDefinition {
    name: String,
    description: String,
    /// JSON Schema for the parameters.
    parameters: serde_json::Value,
}
```

### 1.5 Context (the input to a streaming call)

```rust
struct Context {
    system_prompt: Option<String>,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
}
```

### 1.6 Thinking Level

```rust
/// Serialized in lower-case kebab form: `"minimal"`, `"low"`, `"medium"`,
/// `"high"`, `"xhigh"`. The wire value for `XHigh` matches OpenAI's
/// `reasoning_effort: "xhigh"` exactly.
enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    /// Maximum reasoning effort. Maps to Anthropic adaptive
    /// `output_config: {effort: "max"}` (Opus 4.6 only) and OpenAI
    /// `reasoning_effort: "xhigh"` (GPT-5.2+ only). For models that
    /// don't support this level, falls back to `High`.
    XHigh,
}
```

---

## 2. Streaming Event Protocol (`aj-models::streaming`)

All providers emit events through an `AssistantMessageEventStream`, an async
stream of `AssistantMessageEvent` values. Every event carries a `partial`
clone of the in-progress `AssistantMessage` for snapshot access.

> **Rust note:** `partial` is an owned `AssistantMessage` clone, not a
> reference. Streaming events arrive at network speed so the per-event
> clone cost is negligible.

```rust
enum AssistantMessageEvent {
    /// Stream has started, partial message initialized.
    Start { partial: AssistantMessage },

    /// A new text block started at `content_index`.
    TextStart { content_index: usize, partial: AssistantMessage },
    /// Incremental text delta.
    TextDelta { content_index: usize, delta: String, partial: AssistantMessage },
    /// Text block finalized.
    TextEnd { content_index: usize, content: String, partial: AssistantMessage },

    /// A new thinking block started.
    ThinkingStart { content_index: usize, partial: AssistantMessage },
    /// Incremental thinking delta.
    ThinkingDelta { content_index: usize, delta: String, partial: AssistantMessage },
    /// Thinking block finalized.
    ThinkingEnd { content_index: usize, content: String, partial: AssistantMessage },

    /// A new tool call started.
    ToolCallStart { content_index: usize, partial: AssistantMessage },
    /// Incremental tool call argument delta (partial JSON).
    ToolCallDelta { content_index: usize, delta: String, partial: AssistantMessage },
    /// Tool call finalized with complete parsed arguments.
    ToolCallEnd { content_index: usize, tool_call: ToolCall, partial: AssistantMessage },

    /// Stream completed successfully. `reason` is constrained to successful
    /// terminations: `Stop`, `Length`, or `ToolUse`.
    Done { reason: DoneReason, message: AssistantMessage },
    /// Stream terminated unsuccessfully. `reason` is constrained to failure
    /// terminations: `Error` or `Aborted`.
    Error { reason: ErrorReason, error: AssistantMessage },
}

/// Subset of `StopReason` valid on a `Done` event.
enum DoneReason { Stop, Length, ToolUse }

/// Subset of `StopReason` valid on an `Error` event.
enum ErrorReason { Error, Aborted }
```

### 2.1 Event Stream

```rust
/// Async stream wrapper that also provides a `.result()` future
/// which resolves to the final `AssistantMessage` once the stream
/// emits `Done` or `Error`.
struct AssistantMessageEventStream { ... }

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;
}

impl AssistantMessageEventStream {
    /// Push an event into the stream (producer side).
    fn push(&self, event: AssistantMessageEvent);
    /// Signal end of stream.
    fn end(&self);
    /// Await the final AssistantMessage.
    async fn result(&self) -> AssistantMessage;
}
```

**Terminal events:** `Done` and `Error` are terminal — no further
events may be pushed after either. `result()` resolves to the final
`AssistantMessage` when `Done` is emitted, and to an `AssistantMessage`
with `stop_reason = Error` or `Aborted` when `Error` is emitted. If
the underlying stream ends without emitting either (e.g. a dropped
connection), the provider synthesizes an `Error` event before closing.

**Partial structural tokens in delta text:** text/thinking deltas may
arrive split mid-token in ways that cause user-visible glitches if
emitted verbatim (e.g. a `<thinking>` tag split across two deltas, or
a provider that leaks an opening tag before the thinking channel
begins). Implementations should hold potentially-partial structural
tokens in a small scratch buffer and defer emission until the next
delta disambiguates them.

**Tool call argument deltas:** each `ToolCallDelta` event's `partial`
snapshot must carry the best-effort parse of the arguments accumulated
so far. Implementations use partial-JSON parsing (see §11.1) so that
downstream consumers can read a progressively more complete arguments
object during the stream without waiting for `ToolCallEnd`.

---

## 3. Model Definition & Registry (`aj-models::registry`)

### 3.1 Model Metadata

```rust
struct ModelInfo {
    /// Model identifier sent to the API (e.g. "claude-sonnet-4-20250514").
    id: String,
    /// Human-readable name (e.g. "Claude Sonnet 4").
    name: String,
    /// API type. One of `"anthropic-messages"`, `"openai-completions"`,
    /// or `"openai-responses"`. Each `(provider, id)` pair has exactly
    /// one `api`; there is no duplication (see §3.4.3).
    api: String,
    /// Provider name (e.g. "anthropic", "openai").
    provider: String,
    /// Base URL for API requests.
    base_url: String,
    /// Whether the model supports extended thinking / reasoning.
    reasoning: bool,
    /// Supported input modalities.
    input: Vec<InputModality>,  // Text, Image
    /// Pricing per million tokens.
    cost: ModelCost,
    /// Maximum context window in tokens.
    context_window: u64,
    /// Maximum output tokens.
    max_tokens: u64,
    /// Optional extra HTTP headers.
    headers: Option<HashMap<String, String>>,
}

struct ModelCost {
    input: f64,       // $/million tokens
    output: f64,
    cache_read: f64,
    cache_write: f64,
}

enum InputModality {
    Text,
    Image,
}
```

### 3.2 Registry

The registry holds all known models, organized by provider. It is populated
from a **generated** Rust source file (`models_generated.rs`) that is
produced by a code generation script (see §3.4).

```rust
struct ModelRegistry { ... }

impl ModelRegistry {
    /// Get a model by provider and ID.
    fn get(&self, provider: &str, model_id: &str) -> Option<&ModelInfo>;
    /// List all providers.
    fn providers(&self) -> Vec<&str>;
    /// List all models for a provider.
    fn models(&self, provider: &str) -> Vec<&ModelInfo>;
}
```

Built-in catalog entries include at minimum:

**Anthropic models:** claude-3-5-haiku, claude-3-5-sonnet, claude-3-7-sonnet,
claude-sonnet-4, claude-opus-4, claude-sonnet-4-6, claude-opus-4-6
(with latest aliases).

**OpenAI models:** gpt-4o, gpt-4o-mini, gpt-4.1, gpt-4.1-mini, gpt-4.1-nano,
o1, o1-mini, o3, o3-mini, o3-pro, o4-mini, gpt-5, gpt-5-mini.

Each entry has correct pricing, context window, max tokens, input modalities,
and reasoning capability flag.

### 3.3 Cost Calculation

```rust
fn calculate_cost(model: &ModelInfo, usage: &mut Usage) {
    usage.cost.input = (model.cost.input / 1_000_000.0) * usage.input as f64;
    usage.cost.output = (model.cost.output / 1_000_000.0) * usage.output as f64;
    usage.cost.cache_read = (model.cost.cache_read / 1_000_000.0) * usage.cache_read as f64;
    usage.cost.cache_write = (model.cost.cache_write / 1_000_000.0) * usage.cache_write as f64;
    usage.cost.total = usage.cost.input + usage.cost.output
                     + usage.cost.cache_read + usage.cost.cache_write;
}
```

### 3.3.1 Registry Helpers & Capability Probes

Beyond `get` / `providers` / `models`, the registry exposes a small set
of helpers used by the provider and transform layers:

```rust
/// Structural equality of two ModelInfo references by (provider, api, id).
fn models_are_equal(a: &ModelInfo, b: &ModelInfo) -> bool;

/// Whether the model supports the `XHigh` reasoning level (mapped to
/// Anthropic `effort: "max"` or OpenAI `reasoning_effort: "xhigh"`).
/// Matches by substring on `model.id`:
/// - Anthropic: `"opus-4-6"` or `"opus-4.6"`
/// - OpenAI: `"gpt-5.2"`, `"gpt-5.3"`, `"gpt-5.4"`
/// All other models fall back to `High`.
fn supports_xhigh(model: &ModelInfo) -> bool;
```

Additional capability probes may be added as new tiered features
appear (e.g. `supports_images`, `supports_cache_1h`). Each probe
returns a boolean derived from the static catalog entry — no I/O.

### 3.4 Model Catalog Code Generation

The model catalog is **not** hand-maintained. A code generation script fetches
model metadata from external sources, applies overrides, and writes a Rust
source file that the registry loads at compile time.

#### 3.4.1 Script Location & Invocation

The script lives at `scripts/generate-models.py` and is invoked manually
whenever models need updating:

```bash
python3 scripts/generate-models.py
```

It writes `src/aj-models/src/models_generated.rs`. This generated file is
checked into version control and compiled into the binary — the catalog
is **never** fetched at runtime. The registry has no network dependency
and works fully offline. Updating the catalog is an explicit developer
action (run the script, review the diff, commit).

#### 3.4.2 Data Sources

The script fetches from **[models.dev](https://models.dev/api.json)**, a
community-maintained JSON catalog of LLM metadata. For each provider+model
entry it extracts:

| models.dev field | ModelInfo field |
|---|---|
| `id` | `id` |
| `name` | `name` |
| `tool_call` | (filter: only include if `true`) |
| `reasoning` | `reasoning` |
| `modalities.input` (contains "image") | `input` |
| `cost.input` | `cost.input` ($/million tokens) |
| `cost.output` | `cost.output` |
| `cost.cache_read` | `cost.cache_read` |
| `cost.cache_write` | `cost.cache_write` |
| `limit.context` | `context_window` |
| `limit.output` | `max_tokens` |

The script only processes the `anthropic` and `openai` provider sections
from models.dev (matching our supported providers).

#### 3.4.3 Provider-Specific Mapping

For each provider, the script assigns fixed values:

| Provider | `api` | `base_url` |
|---|---|---|
| `anthropic` | `"anthropic-messages"` | `"https://api.anthropic.com"` |
| `openai` | `"openai-responses"` | `"https://api.openai.com/v1"` |

**One api per `(provider, id)`, no duplication.** The generator emits
exactly one `api` string per `(provider, id)` pair to keep the catalog
unambiguous. Users do not pick between Chat Completions and Responses
for a native OpenAI model; the provider's preferred API is hard-coded
in the catalog. Native `openai` models use `"openai-responses"`.

If in the future we add OpenAI-compatible third-party providers
(Cerebras, Groq, OpenRouter, etc.), those get `"openai-completions"`
because they only speak the Chat Completions shape.

#### 3.4.4 Overrides

The script applies hardcoded overrides after fetching, to fix known
inaccuracies in upstream data or add models that aren't yet in models.dev.
Overrides are documented inline in the script with rationale.

Examples of overrides:
- Fix incorrect cache pricing for specific models
- Override context window for models where upstream data is wrong
- Add brand-new models not yet in models.dev

#### 3.4.5 Generated File Format

The output is a valid Rust source file:

```rust
// This file is auto-generated by scripts/generate-models.py
// Do not edit manually — run `python3 scripts/generate-models.py` to update.

use crate::registry::{ModelInfo, ModelCost, InputModality};

pub fn builtin_models() -> Vec<ModelInfo> {
    vec![
        ModelInfo {
            id: "claude-sonnet-4-20250514".into(),
            name: "Claude Sonnet 4".into(),
            api: "anthropic-messages".into(),
            provider: "anthropic".into(),
            base_url: "https://api.anthropic.com".into(),
            reasoning: true,
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read: 0.3,
                cache_write: 3.75,
            },
            context_window: 200000,
            max_tokens: 64000,
            headers: None,
        },
        // ... more models
    ]
}
```

The `ModelRegistry::new()` constructor calls `builtin_models()` to populate
itself.

#### 3.4.6 Filtering Rules

The script only includes models where:
- `tool_call == true` (we need function calling)
- The provider is `"anthropic"` or `"openai"`

Models without tool calling support are excluded since the agent requires it.

---

## 4. Stream Options

Options passed to any streaming call:

```rust
struct StreamOptions {
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    api_key: Option<String>,
    /// Prompt cache retention preference. Maps to Anthropic
    /// `cache_control.ttl` (`"5m"` / `"1h"`) and to the OpenAI
    /// Responses `prompt_cache_retention` field (`"24h"` on Long).
    /// `openai-completions` ignores this; its cache is implicit.
    cache_retention: CacheRetention,
    /// Session ID for providers that support session-based caching.
    /// Also forwarded as `session_id` and `x-client-request-id`
    /// headers by the `openai-responses` provider (see §7.3).
    session_id: Option<String>,
    /// Extra HTTP headers merged with provider defaults.
    headers: Option<HashMap<String, String>>,
    /// Metadata fields (e.g. Anthropic user_id for rate limiting).
    metadata: Option<HashMap<String, serde_json::Value>>,
    /// Upper bound on retry backoff delay for transient errors.
    /// Default: 60000 (60 seconds). Set to 0 to disable the cap.
    /// If the server's requested delay exceeds this value, the
    /// request fails immediately, allowing higher-level retry logic
    /// to handle it with user visibility.
    max_retry_delay_ms: Option<u64>,
    /// Optional debug callback invoked with the raw outgoing request
    /// body just before it's sent. Useful for logging, recording
    /// test fixtures, or tracing provider-specific payload shape.
    /// Must not mutate the body — providers treat it as read-only.
    on_payload: Option<Arc<dyn Fn(&serde_json::Value) + Send + Sync>>,
    /// Responses-only: request a non-default service tier. Ignored
    /// by non-Responses providers. See §7.3 for cost multipliers.
    service_tier: Option<ServiceTier>,
    /// Responses-only: reasoning summary verbosity. Ignored by
    /// non-Responses providers. Defaults to `Auto` when reasoning
    /// is enabled. See §7.3.2.
    reasoning_summary: Option<ReasoningSummary>,
    /// Controls whether/how the model uses tools.
    /// When `None`, the provider default applies (typically `Auto`).
    tool_choice: Option<ToolChoice>,
}

/// Controls whether the model must, may, or must not use tools.
enum ToolChoice {
    /// Model decides whether to call a tool (default behavior).
    Auto,
    /// Model must not call any tools.
    None,
    /// Model must call at least one tool (any tool).
    Required,
    /// Model must call the specific named tool.
    Tool { name: String },
}

enum CacheRetention {
    None,
    Short,  // default — Anthropic "5m", Responses cache without retention hint
    Long,   // Anthropic "1h", Responses "24h"
}

enum ServiceTier {
    Flex,      // 0.5× cost
    Priority,  // 2× cost
}

enum ReasoningSummary {
    Auto,      // default — let the model decide
    Detailed,  // more verbose reasoning summaries
    Concise,   // shorter reasoning summaries
}

/// Higher-level options that include reasoning control.
struct SimpleStreamOptions {
    base: StreamOptions,
    reasoning: Option<ThinkingLevel>,
}
```

**Cancellation:** There is no explicit cancellation token in `StreamOptions`.
Callers cancel an in-flight request by dropping the `AssistantMessageEventStream`.
Providers must ensure that dropping the stream cancels the underlying HTTP
request (e.g. by aborting the connection future). This is the standard Rust
idiom — `tokio::select!` can race a stream against a shutdown signal.

**Default `max_tokens` resolution:**
- `stream_simple` / `complete_simple`: when `max_tokens` is `None`, defaults
  to `min(model.max_tokens, 32000)`.
- Raw `stream` / `complete`: provider-specific (e.g. Anthropic uses
  `model.max_tokens / 3`, see §6.2).

---

## 5. Provider Trait & Implementations

### 5.1 Provider Trait

```rust
/// A provider knows how to stream inference for a specific API type.
trait Provider: Send + Sync {
    /// Low-level stream with provider-specific options already resolved.
    fn stream(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &StreamOptions,
    ) -> AssistantMessageEventStream;

    /// High-level stream that maps ThinkingLevel to provider-specific config.
    fn stream_simple(
        &self,
        model: &ModelInfo,
        context: &Context,
        options: &SimpleStreamOptions,
    ) -> AssistantMessageEventStream;
}
```

### 5.2 Top-Level API

These functions dispatch to the correct `Provider` implementation based on
`model.api` (not `model.provider`).

```rust
/// Stream inference using the appropriate provider for the model.
fn stream(model: &ModelInfo, context: &Context, options: &StreamOptions)
    -> AssistantMessageEventStream;

/// Stream with simplified reasoning options.
fn stream_simple(model: &ModelInfo, context: &Context, options: &SimpleStreamOptions)
    -> AssistantMessageEventStream;

/// Non-streaming convenience: collect the full AssistantMessage.
async fn complete(model: &ModelInfo, context: &Context, options: &StreamOptions)
    -> AssistantMessage;

async fn complete_simple(model: &ModelInfo, context: &Context, options: &SimpleStreamOptions)
    -> AssistantMessage;
```

---

## 6. Anthropic Provider (`anthropic-sdk` + `aj-models::anthropic`)

### 6.1 SDK Client (`anthropic-sdk`)

The SDK is a thin HTTP client. It handles:
- Building requests to `POST /v1/messages`
- Setting headers: `x-api-key` or `Authorization: Bearer` (for OAuth),
  `anthropic-version`, `content-type`, `anthropic-beta`
- Parsing SSE responses into typed events
- Error handling with typed `ApiError` variants

**Required headers based on auth mode:**

| Auth Mode | Key Header | Extra Headers |
|-----------|-----------|---------------|
| API Key   | `x-api-key: <key>` | `anthropic-beta: fine-grained-tool-streaming-2025-05-14` (add `,interleaved-thinking-2025-05-14` when non-adaptive thinking is enabled) |
| OAuth     | `Authorization: Bearer <token>` | `anthropic-beta: claude-code-20250219,oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14` (add `,interleaved-thinking-2025-05-14` when non-adaptive thinking is enabled), `user-agent: claude-cli/<ver>`, `x-app: cli` |

The `interleaved-thinking-2025-05-14` beta is deprecated on Opus 4.6 and
redundant on Sonnet 4.6 (both have adaptive thinking built in), so it is
only sent for older reasoning models.

The `<ver>` component of the `user-agent` in OAuth mode is a caller-
chosen Claude Code client version string that the Anthropic server
accepts as a recognized Claude Code client. The implementation picks
a concrete recent version; the spec does not fix an exact value.

**OAuth stealth mode:** When using OAuth tokens (prefix `sk-ant-oat`), the
client must:
1. Use `Authorization: Bearer` instead of `x-api-key`
2. Prepend a Claude Code identity system prompt block:
   `"You are Claude Code, Anthropic's official CLI for Claude."`
3. Rename tools to match Claude Code canonical casing on outgoing requests,
   and reverse-map them on responses (so the caller sees their original tool
   names).

The canonical Claude Code 2.x tool list (case-sensitive after mapping):

```
Read, Write, Edit, Bash, Grep, Glob, AskUserQuestion, EnterPlanMode,
ExitPlanMode, KillShell, NotebookEdit, Skill, Task, TaskOutput, TodoWrite,
WebFetch, WebSearch
```

This list reflects Claude Code 2.x and should be updated when major
versions ship new or renamed tools.

Forward mapping (request path): look up the caller's tool name
case-insensitively in this list; if it matches, replace with the canonical
casing. Otherwise pass through unchanged.

Reverse mapping (response path): when a tool_use block arrives with a
canonical name, look it up case-insensitively in the caller-supplied tool
list and replace it with the caller's casing. If no match, pass through.

### 6.2 Provider Implementation

**Message conversion:** Convert unified `Message` types to Anthropic API format:
- `UserMessage` → `{role: "user", content: [text/image blocks]}`
- `AssistantMessage` → `{role: "assistant", content: [text/thinking/tool_use blocks]}`
- `ToolResultMessage` → `{role: "user", content: [{type: "tool_result", ...}]}`
- Consecutive `ToolResultMessage`s are batched into a single user message
- Redacted thinking blocks → `{type: "redacted_thinking", data: ...}`
- Thinking blocks without signatures (from aborted streams) → plain text blocks
- Images as base64 with media type

**Default `max_tokens`:** If `StreamOptions.max_tokens` is `None`, the
provider sends `model.max_tokens / 3` as a reasonable default.

**Prompt caching:**
- Add `cache_control: {type: "ephemeral"}` to system prompt blocks
- Add `cache_control` to the last user message content block
- For `api.anthropic.com` with `Long` retention: add `ttl: "1h"`

**Thinking/reasoning configuration:**
- Models with adaptive thinking (Opus 4.6, Sonnet 4.6): set top-level `thinking: {type: "adaptive"}` and top-level `output_config: {effort: "low"|"medium"|"high"|"max"}` (both are sibling request fields, not nested)
- Older reasoning models: use `thinking: {type: "enabled", budget_tokens: N}`
- Non-reasoning or disabled: `thinking: {type: "disabled"}`
- ThinkingLevel mapping for adaptive: Minimal→low, Low→low, Medium→medium, High→high, XHigh→max (Opus 4.6 only; falls back to high on Sonnet 4.6)
- ThinkingLevel mapping for budget-based: Minimal→1024, Low→2048, Medium→8192, High→16384 (XHigh falls back to High)

**Temperature + extended thinking:** When extended thinking is enabled
(adaptive or budget-based), `temperature` must be omitted from the request.
Anthropic rejects the combination.

**Tool choice mapping:**
- `Auto` → `{type: "auto"}`
- `None` → `{type: "none"}`  (omit from request if no tools are provided)
- `Required` → `{type: "any"}`
- `Tool {name}` → `{type: "tool", name: "<name>"}`

**Stream event mapping:**

| Anthropic SSE | Unified Event |
|---|---|
| `message_start` | `Start`, capture `response_id`, initial usage |
| `content_block_start` (text) | `TextStart` |
| `content_block_start` (thinking) | `ThinkingStart` |
| `content_block_start` (redacted_thinking) | `ThinkingStart` (with redacted flag) |
| `content_block_start` (tool_use) | `ToolCallStart` |
| `content_block_delta` (text_delta) | `TextDelta` |
| `content_block_delta` (thinking_delta) | `ThinkingDelta` |
| `content_block_delta` (input_json_delta) | `ToolCallDelta` (parse partial JSON) |
| `content_block_delta` (signature_delta) | accumulate thinking signature |
| `content_block_stop` | `TextEnd` / `ThinkingEnd` / `ToolCallEnd` |
| `message_delta` | update stop_reason, final usage |

**Usage merging:** Anthropic does not provide `total_tokens`; compute it as
`input + output + cache_read + cache_write`. Usage fields from `message_delta`
should be merged defensively — only update a field when the event value is
non-null. This preserves `message_start` values when proxies omit fields in
`message_delta`.

**Stop reason mapping:**
- `end_turn` → `Stop`
- `max_tokens` → `Length`
- `tool_use` → `ToolUse`
- `refusal` → `Error`
- `pause_turn` → `Stop`
- `stop_sequence` → `Stop` (we don't supply stop sequences, but the API can still return this)
- `sensitive` → `Error` (content flagged by safety filters; not yet in SDK types)

**Tool call argument parsing:** Use incremental/partial JSON parsing so that
`ToolCallDelta` events carry progressively more complete argument objects even
before the JSON is fully received.

---

## 7. OpenAI Provider (`openai-sdk` + `aj-models::openai`)

### 7.1 SDK Client (`openai-sdk`)

The SDK handles:
- `POST /chat/completions` (Chat Completions API)
- `Authorization: Bearer` header
- SSE parsing with `[DONE]` terminator
- Error handling with typed errors

### 7.2 Provider Implementation (Chat Completions)

**Message conversion:** Convert unified types to OpenAI Chat Completions format:
- System prompt → `{role: "developer"}` for reasoning models, `{role: "system"}` otherwise
- `UserMessage` → `{role: "user", content: string | [{type: "text"}, {type: "image_url"}]}`
- `AssistantMessage` → `{role: "assistant", content: string, tool_calls: [...]}`
  - Text content sent as plain string (not array of content blocks)
  - **Thinking blocks are dropped on outbound requests.** The public
    Chat Completions API (`api.openai.com`) does not accept reasoning
    content on input; prior-turn `ThinkingContent` blocks are discarded
    when serializing the history. (The OpenAI Responses provider in §7.3
    preserves them instead.)
  - Empty assistant messages (from aborted streams) are dropped
- `ToolResultMessage` → `{role: "tool", content: string, tool_call_id: string}`
  - Images from tool results: inject a subsequent `user` message with `image_url` parts

**Store:** explicitly set `store: false` on requests. The Chat
Completions API defaults to `false`, but sending it explicitly ensures
conversations are never stored server-side even if the default changes.

**Reasoning effort:**
- Set `reasoning_effort` on the request for models that support it
- ThinkingLevel mapping: Minimal→"low", Low→"low", Medium→"medium", High→"high", XHigh→"xhigh" (GPT-5.2+ only; falls back to "high" on other models)
- Temperature is set normally (not incompatible with reasoning_effort like Anthropic's thinking)

**Tool choice mapping:**
- `Auto` → `"auto"`
- `None` → `"none"`
- `Required` → `"required"`
- `Tool {name}` → `{type: "function", function: {name: "<name>"}}`

**Stream event mapping:**

| OpenAI SSE | Unified Event |
|---|---|
| First chunk | `Start`, capture `response_id` from `chunk.id` |
| `delta.content` | `TextStart` (if new block) + `TextDelta` |
| `delta.reasoning_content` | `ThinkingStart` + `ThinkingDelta` |
| `delta.tool_calls[i]` (with new id) | `ToolCallStart` |
| `delta.tool_calls[i]` (arguments delta) | `ToolCallDelta` |
| `finish_reason` | `TextEnd`/`ThinkingEnd`/`ToolCallEnd` + `Done` |
| `chunk.usage` | parse usage with cached token details |

**Usage parsing:**
- Request `stream_options: {include_usage: true}` for token counts in streaming
- Extract `cached_tokens` from `prompt_tokens_details.cached_tokens`
- Extract `cache_write_tokens` from `prompt_tokens_details.cache_write_tokens`
  (some providers include writes in the cached count)
- `usage.cache_read` = when `cache_write_tokens > 0`:
  `max(0, cached_tokens - cache_write_tokens)`, otherwise `cached_tokens`
- `usage.cache_write` = `cache_write_tokens`
- `usage.input` = `max(0, prompt_tokens - cache_read - cache_write)`
- `usage.output` = `completion_tokens`.
  **Note:** on native OpenAI, `completion_tokens` already includes
  reasoning tokens as a subset. Do not add
  `completion_tokens_details.reasoning_tokens` separately.
- `usage.total_tokens` = `input + output + cache_read + cache_write`
  (compute ourselves; don't trust the provider's `total_tokens`)

**Stop reason mapping:**
- `stop` / `end` → `Stop`
- `length` → `Length`
- `tool_calls` / `function_call` → `ToolUse`
- `content_filter` → `Error` (errorMessage: `"Provider finish_reason: content_filter"`)
- `network_error` → `Error` (errorMessage: `"Provider finish_reason: network_error"`)
- any other value → `Error` (errorMessage: `"Provider finish_reason: {value}"`)

**Tool definition format:**
```json
{"type": "function", "function": {"name": ..., "description": ..., "parameters": ..., "strict": false}}
```

### 7.3 Provider Implementation (Responses)

**API identifier:** `openai-responses` (known `api` string for `ModelInfo`).
**Base URL:** `https://api.openai.com/v1`. **Endpoint:** `POST /responses`.

**Session correlation headers:** when the base URL is `api.openai.com`
and `StreamOptions.session_id` is set, forward it as two request
headers alongside the standard `Authorization` header:
- `session_id: <session_id>`
- `x-client-request-id: <session_id>`

These headers aid server-side correlation for caching and telemetry.
Omit them for non-OpenAI deployments (e.g. Azure) since other endpoints
may reject unknown headers.

#### 7.3.1 Message Conversion

The Responses API uses an `input` array of typed items instead of a flat
`messages` array. Unified types map as follows:

- System prompt → `{role: "developer", content: "..."}` for reasoning models,
  `{role: "system", content: "..."}` otherwise
- `UserMessage` → `{role: "user", content: [{type: "input_text", text: "..."}]}`
  - Images: `{type: "input_image", detail: "auto", image_url: "data:...;base64,..."}`
- `AssistantMessage` → expanded into multiple input items:
  - **Thinking blocks with signature**: deserialize `thinking_signature` back
    into a full `reasoning` input item (the signature contains the serialized
    `ResponseReasoningItem` JSON — see §7.3.3)
  - **Text blocks**: `{type: "message", role: "assistant", id: <msg_id>,
    content: [{type: "output_text", text: "..."}], status: "completed"}`
    where `msg_id` is extracted from `text_signature` (see §7.3.4)
  - **Tool calls**: `{type: "function_call", id: <item_id>, call_id: <call_id>,
    name: "...", arguments: "..."}` — the composite ID stored in `ToolCall.id`
    is split on `|` to recover `call_id` and `item_id` (see §7.3.5)
  - **Cross-model replay:** reasoning item IDs (carried in
    `thinking_signature`) are tied to a specific server-side response
    chain. They cannot be replayed against a different model **even
    within the same `provider + api`** (e.g. `gpt-5` → `gpt-5-mini`).
    Treat any change in `model.id` as "different model" for the
    purpose of dropping thinking blocks with signatures, regardless of
    whether provider and api match. Additionally, for tool calls from
    a different model within the same provider+api, omit the `id`
    field entirely from the outbound `function_call` input item (keep
    `call_id`) to avoid server-side pairing validation between `fc_xxx`
    IDs and `rs_xxx` reasoning items.
  - Thinking blocks without signatures are **dropped** (cannot be round-tripped)
  - Empty assistant messages are skipped
- `ToolResultMessage` → `{type: "function_call_output", call_id: <call_id>,
  output: "..."}`
  - `call_id` is extracted by splitting `tool_call_id` on `|` and taking
    the first part
  - Images are supported inline: `output` becomes an array of
    `input_text` and `input_image` items

#### 7.3.2 Request Parameters

```
POST /responses
{
  model: <model_id>,
  input: <converted messages>,
  stream: true,
  store: false,
  prompt_cache_key: <session_id or omit>,
  prompt_cache_retention: <"24h" or omit>,
  service_tier: <"flex" | "priority" or omit>,
  temperature: <if set>,
  max_output_tokens: <if set>,
  tools: [{type: "function", name, description, parameters, strict: false}],
  tool_choice: <tool_choice mapping or omit>,
  reasoning: {effort: <level>, summary: <reasoning_summary or "auto">},
  include: ["reasoning.encrypted_content"],  // when reasoning is enabled
}
```

**Service tier:** the Responses API accepts an optional `service_tier`
parameter that trades latency/availability against cost. Surface this
as an optional field on `StreamOptions` (specific to the Responses
provider; other providers ignore it):

| Tier | Wire value | Cost multiplier |
|---|---|---|
| Flex | `"flex"` | 0.5× |
| Priority | `"priority"` | 2× |
| Standard (default) | omit | 1× |

When a non-default tier is used, the provider multiplies the computed
`usage.cost.{input, output, cache_read, cache_write, total}` by the
tier's factor after the base cost calculation in §3.3, before returning
the final `Usage`. When applying the tier multiplier, use the
`service_tier` value from `response.completed` if present, falling back
to the requested tier from `StreamOptions`. The server may assign a
different tier than requested.

**Prompt caching:**
- `prompt_cache_key`: set to `StreamOptions.session_id` when
  `cache_retention != None`, omitted otherwise
- `prompt_cache_retention`: set to `"24h"` when `cache_retention == Long`
  and base URL is `api.openai.com`, omitted otherwise

**Store:** hardcoded to `false`. Server-side conversation storage is not
used currently. If enabled in the future, it would allow use of
`previous_response_id` for server-side conversation chaining (the API
resumes from a prior response, avoiding resending the full history).
Both are Responses-specific concerns and do not belong in the base
`StreamOptions`.

**Reasoning configuration:**
- When reasoning is requested: `reasoning: {effort: <mapped_level>, summary: <reasoning_summary or "auto">}`
  and `include: ["reasoning.encrypted_content"]` to receive encrypted reasoning
  for multi-turn continuity
- When reasoning is not requested: `reasoning: {effort: "none"}`
- ThinkingLevel mapping: same as §7.2 (Minimal→"low", ..., XHigh→"xhigh")

**Tool choice mapping:** same wire format as Chat Completions (§7.2).

**Tool definition format:**
```json
{"type": "function", "name": ..., "description": ..., "parameters": ..., "strict": false}
```

#### 7.3.3 Reasoning Round-Trip

Prior-turn `ThinkingContent` blocks are preserved across turns, unlike
Chat Completions (§7.2) where they are dropped.

**On output (stream → unified types):** when a `reasoning` output item
completes, the full `ResponseReasoningItem` object (including `id`,
`type`, `summary`, and any encrypted content) is serialized to JSON and
stored in `ThinkingContent.thinking_signature`. The visible reasoning
summary text goes into `ThinkingContent.thinking`.

**On input (unified types → API request):** `thinking_signature` is
deserialized back into a `ResponseReasoningItem` and inserted directly
into the `input` array as a `reasoning` item. This preserves the
encrypted reasoning chain across turns.

#### 7.3.4 Text Signatures

The Responses API assigns each output message an `id` and optional
`phase` (`"commentary"` or `"final_answer"`). These are stored in
`TextContent.text_signature` as a JSON-encoded `TextSignatureV1`:

```rust
struct TextSignatureV1 {
    v: u8,       // always 1
    id: String,  // e.g. "msg_abc123"
    phase: Option<String>,  // "commentary" or "final_answer"
}
```

**On output:** encode `{v: 1, id: <item.id>, phase: <item.phase>}` as
JSON and store in `TextContent.text_signature`.

**On input:** determine how to populate the outgoing `message` item's
`id` and `phase` fields from the stored `text_signature`:

1. **Signature is a valid `TextSignatureV1` JSON:** parse it; use
   `id` (truncated to 64 chars, or replaced with `msg_<short_hash(id)>`
   if it would exceed the limit) and pass `phase` through unchanged.
2. **Signature is present but does not parse as `TextSignatureV1`:**
   treat the entire string as a legacy plain id — truncate/hash as
   above; leave `phase` unset.
3. **Signature is absent:** omit `id` from the outgoing `message`
   item and leave `phase` unset. The server will accept the message
   but cannot pair it with a specific prior output.

#### 7.3.5 Composite Tool Call IDs

The Responses API uses two IDs per tool call: a `call_id` (used to match
function call outputs) and an `id` (the output item ID, prefixed `fc_`).
These are stored together in `ToolCall.id` as `{call_id}|{item_id}`.

**On output:** `ToolCall.id = "{item.call_id}|{item.id}"`.
**On input:** split on `|` to recover `call_id` and `id` for the
`function_call` input item. For `function_call_output`, only `call_id`
is needed.

When normalizing foreign tool call IDs (from other providers) for the
Responses API, ensure `item_id` starts with `fc_` as required by the
API. If the input ID is not already in the correct form, replace it
with `fc_<short_hash(id)>` — a short stable hash of the original ID
(e.g. first 12 hex chars of a SHA-256 digest). This keeps the wire
format valid without trying to preserve semantic continuity with a
different provider's IDs.

#### 7.3.6 Stream Event Mapping

| Responses SSE | Unified Event |
|---|---|
| `response.created` | capture `response_id` |
| `response.output_item.added` (reasoning) | `ThinkingStart` |
| `response.output_item.added` (message) | `TextStart` |
| `response.output_item.added` (function_call) | `ToolCallStart` (with composite ID) |
| `response.content_part.added` | track content part within message item (accept `output_text` and `refusal` only) |
| `response.reasoning_summary_text.delta` | `ThinkingDelta` |
| `response.reasoning_summary_part.done` | `ThinkingDelta` with `"\n\n"` separator between summary parts |
| `response.output_text.delta` | `TextDelta` |
| `response.refusal.delta` | `TextDelta` (treated as text) |
| `response.function_call_arguments.delta` | `ToolCallDelta` (partial JSON parse) |
| `response.function_call_arguments.done` | emit remaining delta if any |
| `response.output_item.done` (reasoning) | `ThinkingEnd` (serialize item to `thinking_signature`) |
| `response.output_item.done` (message) | `TextEnd` (encode `TextSignatureV1` to `text_signature`) |
| `response.output_item.done` (function_call) | `ToolCallEnd` (final parse of arguments) |
| `response.completed` | parse usage, set stop reason, emit `Done` |
| `response.failed` | extract `response.error.{code, message}` if present, otherwise fall back to `response.incomplete_details.reason`; throw |
| `error` | throw with `code` and `message` from event |

**Reasoning summary parts:** reasoning items stream their summary via
`response.reasoning_summary_part.added` and
`response.reasoning_summary_text.delta` events. Summary parts are
separated by `\n\n`. The `thinking` field accumulates all summary text;
the full `ResponseReasoningItem` (with summary array) is captured on
`response.output_item.done`.

#### 7.3.7 Usage Parsing

Usage arrives in `response.completed` → `response.usage`:
- `input_tokens` includes cached tokens; subtract
  `input_tokens_details.cached_tokens` for non-cached input
- `output_tokens` maps directly to `usage.output`
- `total_tokens` maps to `usage.total_tokens`
- `cache_write` is 0 (Responses API does not report cache writes separately)

#### 7.3.8 Stop Reason Mapping

The Responses API uses `response.status` instead of `finish_reason`:

| Response status | Stop Reason |
|---|---|
| `completed` | `Stop` (override to `ToolUse` if content contains tool calls) |
| `incomplete` | `Length` |
| `failed` | `Error` |
| `cancelled` | `Error` |
| `in_progress` | `Stop` (should not appear on a finished response; handle defensively) |
| `queued` | `Stop` (should not appear on a finished response; handle defensively) |

All other concerns (auth, cost calculation, context-overflow detection,
partial-JSON parsing, tool call ID normalization) reuse the mechanisms
in §3, §8, §10, and §11.

---

## 8. Cross-Provider Message Transformation

When replaying a conversation that was partially generated by a different
provider/model, messages must be transformed for compatibility.

### 8.1 Transformation Rules

```rust
fn transform_messages(messages: &[Message], target_model: &ModelInfo) -> Vec<Message>;
```

**Implementation shape — two passes:**

1. **Pass 1** walks every message, rewrites assistant content according
   to the rules below, and builds a `tool_call_id` → `normalized_id`
   map for every tool call emitted by the (kept) assistant messages.
2. **Pass 2** walks again to finalize tool-result alignment: each
   `ToolResultMessage` has its `tool_call_id` rewritten via the map;
   any assistant tool call with no matching result gets a synthetic
   error result appended (see rule 4); and errored/aborted assistant
   messages (see rule 5) are dropped along with any results that
   referenced them.

Two passes are necessary because orphan detection and ID rewriting
both depend on the complete set of kept tool calls, which isn't known
until pass 1 finishes.

For each assistant message in the history:

1. **Same model** (same provider + api + model id): pass through unchanged,
   preserve all signatures and thinking blocks — including thinking blocks
   with empty text but a valid signature (e.g. OpenAI Responses encrypted
   reasoning items that have no visible text but must be round-tripped).
2. **Different model:**
   - Redacted thinking blocks: **drop** (they're encrypted for the original model)
   - Thinking blocks with signatures but same provider+api: keep (needed
     for replay), including those with empty thinking text
   - Thinking blocks with empty text and no signature: **drop**
   - Thinking blocks with non-empty text but no signature: convert to plain text
   - Text blocks: strip `text_signature` (provider-specific)
   - Tool calls: normalize IDs to target provider's format
   - **`openai-responses`:** additional cross-model replay constraints
     apply during message conversion — see §7.3.1 "Cross-model replay".

3. **Tool call ID normalization:**
   - Anthropic: IDs must match `^[a-zA-Z0-9_-]+$`, max 64 chars
   - OpenAI: IDs should be ≤40 chars
   - Normalize by replacing invalid chars with `_`, truncating

4. **Orphaned tool calls:** If an assistant message has tool calls but the
   conversation has no corresponding `ToolResultMessage`, insert a synthetic
   error tool result: `{content: "No result provided", is_error: true}`.

5. **Errored/aborted assistant messages (pass 2):** Skip entirely
   (stopReason == Error or Aborted), along with any `ToolResultMessage`s
   that referenced their tool calls. These are incomplete turns that
   shouldn't be replayed. This is applied in pass 2, not pass 1,
   because skipping the assistant message must also remove its
   associated tool results.

---

## 9. Authentication & API Key Resolution

> **Note on provenance:** The `AuthStorage` design in §9.1 is original
> and not derived from the provider SDK patterns we referenced elsewhere.
> Those SDKs delegate credential persistence to higher-level callers.
> We bake it into `aj-models::auth` so the CLI and agent share one
> implementation.

### 9.1 Auth Storage

Credentials are persisted in `~/.aj/auth.json` with file-level
locking to prevent race conditions when multiple instances refresh tokens
simultaneously.

```rust
enum AuthCredential {
    ApiKey { key: String },
    OAuth {
        refresh: String,
        access: String,
        expires: i64,  // unix ms
        // Provider-specific extra fields (e.g. accountId for OpenAI)
        extra: HashMap<String, serde_json::Value>,
    },
}

struct AuthStorage {
    // Keyed by provider name
    credentials: HashMap<String, AuthCredential>,
}
```

**API key resolution priority:**
1. Runtime override (e.g. CLI `--api-key` flag)
2. API key from `auth.json`
3. OAuth token from `auth.json` (auto-refreshed if expired)
4. Environment variable (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, etc.)

### 9.2 OAuth Provider Trait

```rust
trait OAuthProvider: Send + Sync {
    fn id(&self) -> &str;
    fn name(&self) -> &str;

    /// Run the full login flow. Returns credentials to persist.
    async fn login(&self, callbacks: &dyn OAuthCallbacks) -> Result<OAuthCredentials>;

    /// Refresh expired credentials.
    async fn refresh_token(&self, credentials: &OAuthCredentials) -> Result<OAuthCredentials>;

    /// Extract the API key/token from credentials.
    fn get_api_key(&self, credentials: &OAuthCredentials) -> String;
}

struct OAuthCredentials {
    refresh: String,
    access: String,
    expires: i64,
    extra: HashMap<String, serde_json::Value>,
}

trait OAuthCallbacks: Send + Sync {
    /// Called with the authorization URL the user should open.
    fn on_auth(&self, url: &str, instructions: Option<&str>);
    /// Prompt the user for text input (e.g. paste authorization code).
    async fn on_prompt(&self, message: &str) -> Result<String>;
    /// Optional progress messages.
    fn on_progress(&self, message: &str);
    /// Optional: get manual code input (races with browser callback).
    async fn on_manual_code_input(&self) -> Result<String>;
}
```

### 9.3 Anthropic OAuth Flow

**Protocol:** OAuth 2.0 Authorization Code + PKCE

1. Generate PKCE verifier + challenge (SHA-256, base64url)
2. Start local HTTP callback server on `127.0.0.1:53692` (this port is
   fixed by the upstream OAuth server's allowed redirect URIs; it is
   not an implementation choice)
3. Construct authorization URL:
   - Endpoint: `https://claude.ai/oauth/authorize`
   - Params: `code=true`, `client_id`, `response_type=code`, `redirect_uri=http://localhost:53692/callback`,
     `scope=org:create_api_key user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload`,
     `code_challenge`, `code_challenge_method=S256`, `state=<verifier>`
   - Note: passing the PKCE verifier as the `state` parameter is intentional
     for Claude Code server compatibility — do not "fix" this to a random
     value or the server will reject the callback.
4. Open URL in browser, wait for callback or manual paste
5. Exchange code at `https://platform.claude.com/v1/oauth/token`
6. Store `{refresh, access, expires}` in auth.json

**Token refresh:** POST to the same token URL with `grant_type=refresh_token`.

**OAuth token detection:** Access tokens have prefix `sk-ant-oat`.

### 9.4 OpenAI OAuth Flow (ChatGPT/Codex)

**Protocol:** OAuth 2.0 Authorization Code + PKCE

1. Generate PKCE verifier + challenge
2. Start local HTTP callback server on `127.0.0.1:1455` (this port is
   fixed by the upstream OAuth server's allowed redirect URIs; it is
   not an implementation choice)
3. Construct authorization URL:
   - Endpoint: `https://auth.openai.com/oauth/authorize`
   - Params: `client_id=app_EMoamEEZ73f0CkXaXp7hrann`, `response_type=code`,
     `redirect_uri=http://localhost:1455/auth/callback`,
     `scope=openid profile email offline_access`,
     `code_challenge`, `code_challenge_method=S256`, `state=<random>`,
     `id_token_add_organizations=true`, `codex_cli_simplified_flow=true`,
     `originator=<client-name>` (caller-chosen identifier for the
     client application; we use `aj`)
4. Exchange code at `https://auth.openai.com/oauth/token` (form-urlencoded)
5. The returned **access token** is itself a JWT. Base64-decode the payload
   segment and read `claims["https://api.openai.com/auth"].chatgpt_account_id`
   to obtain the account ID.
6. Store `{refresh, access, expires, accountId}` in auth.json

**Token refresh:** POST form-urlencoded with `grant_type=refresh_token`.

### 9.5 Environment Variable Mapping

| Provider | Environment Variable |
|----------|---------------------|
| anthropic | `ANTHROPIC_API_KEY` (also check `ANTHROPIC_OAUTH_TOKEN` first) |
| openai | `OPENAI_API_KEY` |

---

## 10. Context Overflow Detection

```rust
/// Check if an AssistantMessage represents a context overflow error.
fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool;
```

Detection has two cases:

### 10.1 Error-based overflow

When `stop_reason == Error` and `error_message` is present, test against
overflow patterns. Before matching, first check **non-overflow exclusion
patterns** — some errors (rate limiting, throttling) contain words like
"too many tokens" but are not context overflow.

**Non-overflow exclusion patterns** (skip overflow detection if matched):
- `/rate limit/i`
- `/too many requests/i`

**Overflow patterns** (match after exclusion check passes):

| Pattern | Provider |
|---|---|
| `prompt is too long` | Anthropic token overflow |
| `request_too_large` | Anthropic request byte-size overflow (HTTP 413) |
| `exceeds the context window` | OpenAI (Completions & Responses) |
| `context[_ ]length[_ ]exceeded` | Generic fallback |
| `too many tokens` | Generic fallback |
| `token limit exceeded` | Generic fallback |
| `maximum context length is \d+ tokens` | Generic fallback |
| `reduce the length of the messages` | Generic fallback |

Additional patterns can be added as we support more providers.

### 10.2 Silent overflow

Some providers accept overflowing requests without error. Detect this
when `stop_reason == Stop` and `usage.input + usage.cache_read >
context_window` (requires the caller to pass the model's context window).

---

## 11. Utility Functions

### 11.1 Partial JSON Parsing

For streaming tool call arguments, parse incomplete JSON incrementally:

```rust
/// Parse potentially incomplete JSON. Returns the most complete
/// value possible, or an empty object if parsing fails entirely.
fn parse_streaming_json(partial: &str) -> serde_json::Value;
```

Implementation: try `serde_json::from_str` first, fall back to a
partial JSON parser that handles unclosed strings, objects, arrays.

---

## 12. Implementation Plan

All work happens across three crates: `anthropic-sdk`, `openai-sdk`, and
`aj-models`. The plan is ordered by dependency.

### Phase 1: Unified Types (aj-models)

1. **Define new type module** (`aj-models::types`): Create all types from
   §1 (TextContent, ThinkingContent, ImageContent, ToolCall, UserMessage,
   AssistantMessage, ToolResultMessage, Message, Usage, UsageCost,
   StopReason, ToolDefinition, Context, ThinkingLevel, CacheRetention,
   StreamOptions, SimpleStreamOptions). All types derive Serialize,
   Deserialize, Clone, Debug.

2. **Define streaming event types** (`aj-models::streaming`): Replace the
   current `StreamingEvent` enum with `AssistantMessageEvent` from §2.
   Implement `AssistantMessageEventStream` as an async channel-backed
   stream with a `result()` future.

3. **Define model metadata and registry** (`aj-models::registry`): Create
   `ModelInfo`, `ModelCost`, `InputModality`, `ModelRegistry` from §3.
   Implement `calculate_cost`.

3b. **Create model catalog generator** (`scripts/generate-models.py`): Write
    the Python script per §3.4 that fetches from models.dev, filters to
    Anthropic + OpenAI tool-capable models, applies overrides, and writes
    `src/aj-models/src/models_generated.rs`. Run it once to produce the
    initial generated file and check it in.

4. **Define provider trait** (`aj-models::provider`): Create the `Provider`
   trait from §5.1 and the top-level `stream`/`stream_simple`/`complete`/
   `complete_simple` functions from §5.2.

### Phase 2: Anthropic Provider

5. **Update `anthropic-sdk`**: Adjust the client to support both API key
   and OAuth bearer token auth modes. Add the required beta headers.
   Add OAuth stealth mode (Claude Code tool renaming, identity headers).
   Keep the SDK thin — it sends HTTP requests and parses SSE.

6. **Implement Anthropic provider** (`aj-models::anthropic`): Rewrite
   as an implementation of the `Provider` trait. Implement:
   - Message conversion (unified → Anthropic API format)
   - Prompt caching (cache_control on system + last user message)
   - Thinking configuration (adaptive vs budget-based)
   - SSE → AssistantMessageEvent mapping
   - Partial JSON parsing for tool call arguments
   - Stop reason mapping
   - Usage/cost tracking

### Phase 3: OpenAI Provider

7. **Update `openai-sdk`**: Ensure the client supports all needed fields
   (reasoning_effort, stream_options with include_usage, developer role).
   The SDK should remain a thin HTTP+SSE client.

8. **Implement OpenAI Chat Completions provider** (`aj-models::openai`):
   Rewrite as an implementation of the `Provider` trait. Implement:
   - Message conversion (unified → OpenAI Chat Completions format)
   - System prompt as developer/system role based on reasoning capability
   - Reasoning effort mapping from ThinkingLevel
   - SSE → AssistantMessageEvent mapping
   - Reasoning content field detection (`delta.reasoning_content`)
   - Tool call streaming with partial JSON parsing
   - Usage parsing with cached token subtraction
   - Stop reason mapping

### Phase 4: Cross-Provider & Utilities

9. **Message transformation** (`aj-models::transform`): Implement
   `transform_messages` from §8 — handle cross-provider replay,
   thinking block conversion, tool call ID normalization, orphaned
   tool call synthetic results, errored message skipping.

10. **Partial JSON parser**: Implement or integrate a partial JSON
    parsing library for streaming tool call arguments.

11. **Context overflow detection**: Implement `is_context_overflow` from §10.

### Phase 5: Authentication

12. **OAuth infrastructure** (`aj-models::oauth`): Implement the
    `OAuthProvider` trait, `OAuthCredentials`, `OAuthCallbacks` from §9.2.
    Implement PKCE utilities (verifier generation, SHA-256 challenge).

13. **Anthropic OAuth** (`aj-models::oauth::anthropic`): Implement the
    full Anthropic OAuth flow from §9.3 — local callback server,
    authorization URL construction, token exchange, token refresh.

14. **OpenAI OAuth** (`aj-models::oauth::openai`): Implement the OpenAI
    Codex OAuth flow from §9.4 — local callback server, authorization
    URL construction, token exchange with form-urlencoded body, JWT
    parsing for account ID, token refresh.

15. **Auth storage** (`aj-models::auth`): Implement `AuthStorage` from
    §9.1 — JSON file persistence with file locking, credential CRUD,
    API key resolution with priority chain, automatic OAuth token refresh.

### Phase 6: Integration

16. **Update `aj-agent`**: Migrate the agent loop to use the new unified
    types and streaming protocol. Replace direct `Model` trait usage with
    `stream_simple()` calls. Update conversation persistence to use the
    new `Message` types. Wire up auth storage for API key resolution.

17. **Update `aj` CLI**: Add `--provider` flag alongside existing
    `--model_api`. Add `/login` command support. Wire up model registry
    for model selection and validation.

18. **Remove old code**: Remove the `messages` module (replaced by
    `types`), remove the old `Model` trait and `create_model` function,
    remove the old `StreamingEvent` enum, remove the `openai_ng` module,
    remove `conversation.rs` message types (replaced by unified types).
    Remove the `async-openai` dependency.
