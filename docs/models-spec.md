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
    /// Opaque provider-specific signature for multi-turn replay.
    /// Anthropic: unused. OpenAI Responses: JSON-encoded TextSignatureV1.
    text_signature: Option<String>,
}

/// Extended thinking / reasoning content.
struct ThinkingContent {
    thinking: String,
    /// Opaque signature for multi-turn replay (Anthropic: base64 signature,
    /// OpenAI: reasoning item JSON).
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
    content: Vec<UserContent>,  // or just a String convenience
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
enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}
```

---

## 2. Streaming Event Protocol (`aj-models::streaming`)

All providers emit events through an `AssistantMessageEventStream`, an async
stream of `AssistantMessageEvent` values. Every event carries a `partial`
reference to the in-progress `AssistantMessage` for snapshot access.

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

    /// Stream completed successfully.
    Done { reason: StopReason, message: AssistantMessage },
    /// Stream terminated with an error.
    Error { reason: StopReason, error: AssistantMessage },
}
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

---

## 3. Model Definition & Registry (`aj-models::registry`)

### 3.1 Model Metadata

```rust
struct ModelInfo {
    /// Model identifier sent to the API (e.g. "claude-sonnet-4-20250514").
    id: String,
    /// Human-readable name (e.g. "Claude Sonnet 4").
    name: String,
    /// API type (e.g. "anthropic-messages", "openai-completions").
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

/// Calculate cost from model pricing and usage.
fn calculate_cost(model: &ModelInfo, usage: &mut Usage);
```

Built-in catalog entries include at minimum:

**Anthropic models:** claude-3-5-haiku, claude-3-5-sonnet, claude-3-7-sonnet,
claude-sonnet-4, claude-opus-4, claude-sonnet-4-6, claude-opus-4-6
(with latest aliases).

**OpenAI models:** gpt-4o, gpt-4o-mini, gpt-4.1, gpt-4.1-mini, gpt-4.1-nano,
o1, o1-mini, o3, o3-mini, o3-pro, o4-mini, gpt-5, gpt-5-mini.

Each entry has correct pricing, context window, max tokens, input modalities,
and reasoning capability flag.

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
checked into version control so that builds don't require network access.

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
| `openai` | `"openai-completions"` | `"https://api.openai.com/v1"` |

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

---

## 4. Stream Options

Options passed to any streaming call:

```rust
struct StreamOptions {
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    api_key: Option<String>,
    /// Prompt cache retention preference.
    cache_retention: CacheRetention,
    /// Session ID for providers that support session-based caching.
    session_id: Option<String>,
    /// Extra HTTP headers merged with provider defaults.
    headers: Option<HashMap<String, String>>,
    /// Metadata fields (e.g. Anthropic user_id for rate limiting).
    metadata: Option<HashMap<String, serde_json::Value>>,
}

enum CacheRetention {
    None,
    Short,  // default
    Long,
}

/// Higher-level options that include reasoning control.
struct SimpleStreamOptions {
    base: StreamOptions,
    reasoning: Option<ThinkingLevel>,
}
```

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
| API Key   | `x-api-key: <key>` | `anthropic-beta: fine-grained-tool-streaming-2025-05-14` |
| OAuth     | `Authorization: Bearer <token>` | `anthropic-beta: claude-code-20250219,oauth-2025-04-20,...`, `user-agent: claude-cli/<ver>`, `x-app: cli` |

**OAuth stealth mode:** When using OAuth tokens (prefix `sk-ant-oat`), the
client must:
1. Use `Authorization: Bearer` instead of `x-api-key`
2. Prepend a Claude Code identity system prompt block
3. Rename tools to match Claude Code canonical casing (Read, Write, Edit,
   Bash, Grep, Glob, etc.) and reverse-map them on responses

### 6.2 Provider Implementation

**Message conversion:** Convert unified `Message` types to Anthropic API format:
- `UserMessage` → `{role: "user", content: [text/image blocks]}`
- `AssistantMessage` → `{role: "assistant", content: [text/thinking/tool_use blocks]}`
- `ToolResultMessage` → `{role: "user", content: [{type: "tool_result", ...}]}`
- Consecutive `ToolResultMessage`s are batched into a single user message
- Redacted thinking blocks → `{type: "redacted_thinking", data: ...}`
- Thinking blocks without signatures (from aborted streams) → plain text blocks
- Images as base64 with media type

**Prompt caching:**
- Add `cache_control: {type: "ephemeral"}` to system prompt blocks
- Add `cache_control` to the last user message content block
- For `api.anthropic.com` with `Long` retention: add `ttl: "1h"`

**Thinking/reasoning configuration:**
- Models with adaptive thinking (Opus 4.6, Sonnet 4.6): use `thinking: {type: "adaptive"}` with `output_config: {effort: "low"|"medium"|"high"|"max"}`
- Older reasoning models: use `thinking: {type: "enabled", budget_tokens: N}`
- Non-reasoning or disabled: `thinking: {type: "disabled"}`
- ThinkingLevel mapping for adaptive: Minimal→low, Low→low, Medium→medium, High→high
- ThinkingLevel mapping for budget-based: Minimal→1024, Low→2048, Medium→8192, High→16384

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

**Stop reason mapping:**
- `end_turn` → `Stop`
- `max_tokens` → `Length`
- `tool_use` → `ToolUse`
- `refusal` → `Error`
- `pause_turn` → `Stop`

**Tool call argument parsing:** Use incremental/partial JSON parsing so that
`ToolCallDelta` events carry progressively more complete argument objects even
before the JSON is fully received.

**Unicode sanitization:** Strip unpaired Unicode surrogates from all text
before sending to the API (they cause JSON serialization failures).

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
  - Thinking blocks: stored via the `reasoning_content` / `reasoning` field name from the original streaming field, or converted to plain text for cross-provider replay
  - Empty assistant messages (from aborted streams) are dropped
- `ToolResultMessage` → `{role: "tool", content: string, tool_call_id: string}`
  - Images from tool results: inject a subsequent `user` message with `image_url` parts

**Reasoning effort:**
- Set `reasoning_effort` on the request for models that support it
- ThinkingLevel mapping: Minimal→"low", Low→"low", Medium→"medium", High→"high"
- Temperature is set normally (not incompatible with reasoning_effort like Anthropic's thinking)

**Stream event mapping:**

| OpenAI SSE | Unified Event |
|---|---|
| First chunk | `Start`, capture `response_id` from `chunk.id` |
| `delta.content` | `TextStart` (if new block) + `TextDelta` |
| `delta.reasoning_content` / `delta.reasoning` | `ThinkingStart` + `ThinkingDelta` |
| `delta.tool_calls[i]` (with new id) | `ToolCallStart` |
| `delta.tool_calls[i]` (arguments delta) | `ToolCallDelta` |
| `finish_reason` | `TextEnd`/`ThinkingEnd`/`ToolCallEnd` + `Done` |
| `chunk.usage` | parse usage with cached token details |

**Usage parsing:**
- `prompt_tokens` includes cached tokens; subtract `prompt_tokens_details.cached_tokens` for non-cached input
- Add `completion_tokens_details.reasoning_tokens` to output
- Request `stream_options: {include_usage: true}` for token counts in streaming

**Stop reason mapping:**
- `stop` / `end` → `Stop`
- `length` → `Length`
- `tool_calls` / `function_call` → `ToolUse`
- `content_filter` → `Error`

**Tool definition format:**
```json
{"type": "function", "function": {"name": ..., "description": ..., "parameters": ..., "strict": false}}
```

---

## 8. Cross-Provider Message Transformation

When replaying a conversation that was partially generated by a different
provider/model, messages must be transformed for compatibility.

### 8.1 Transformation Rules

```rust
fn transform_messages(messages: &[Message], target_model: &ModelInfo) -> Vec<Message>;
```

For each assistant message in the history:

1. **Same model** (same provider + api + model id): pass through unchanged,
   preserve all signatures and thinking blocks.
2. **Different model:**
   - Redacted thinking blocks: **drop** (they're encrypted for the original model)
   - Thinking blocks with signatures but same provider+api: keep (needed for replay)
   - Thinking blocks without signatures: convert to plain text
   - Text blocks: strip `text_signature` (provider-specific)
   - Tool calls: normalize IDs to target provider's format, drop `thought_signature`

3. **Tool call ID normalization:**
   - Anthropic: IDs must match `^[a-zA-Z0-9_-]+$`, max 64 chars
   - OpenAI: IDs should be ≤40 chars
   - Normalize by replacing invalid chars with `_`, truncating

4. **Orphaned tool calls:** If an assistant message has tool calls but the
   conversation has no corresponding `ToolResultMessage`, insert a synthetic
   error tool result: `{content: "No result provided", is_error: true}`.

5. **Errored/aborted assistant messages:** Skip entirely (stopReason == Error
   or Aborted). These are incomplete turns that shouldn't be replayed.

---

## 9. Authentication & API Key Resolution

### 9.1 Auth Storage

Credentials are persisted in `~/.config/aj/auth.json` with file-level
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
2. Start local HTTP callback server on `127.0.0.1:53692`
3. Construct authorization URL:
   - Endpoint: `https://claude.ai/oauth/authorize`
   - Params: `client_id`, `response_type=code`, `redirect_uri=http://localhost:53692/callback`,
     `scope=org:create_api_key user:profile user:inference user:sessions:claude_code ...`,
     `code_challenge`, `code_challenge_method=S256`, `state=<verifier>`
4. Open URL in browser, wait for callback or manual paste
5. Exchange code at `https://platform.claude.com/v1/oauth/token`
6. Store `{refresh, access, expires}` in auth.json

**Token refresh:** POST to the same token URL with `grant_type=refresh_token`.

**OAuth token detection:** Access tokens have prefix `sk-ant-oat`.

### 9.4 OpenAI OAuth Flow (ChatGPT/Codex)

**Protocol:** OAuth 2.0 Authorization Code + PKCE

1. Generate PKCE verifier + challenge
2. Start local HTTP callback server on `127.0.0.1:1455`
3. Construct authorization URL:
   - Endpoint: `https://auth.openai.com/oauth/authorize`
   - Params: `client_id=app_EMoamEEZ73f0CkXaXp7hrann`, `response_type=code`,
     `redirect_uri=http://localhost:1455/auth/callback`,
     `scope=openid profile email offline_access`,
     `code_challenge`, `code_challenge_method=S256`, `state=<random>`,
     `codex_cli_simplified_flow=true`
4. Exchange code at `https://auth.openai.com/oauth/token` (form-urlencoded)
5. Decode JWT to extract `chatgpt_account_id` from claims
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

Detection via regex patterns on `error_message` when `stop_reason == Error`:
- `prompt is too long` (Anthropic)
- `exceeds the context window` (OpenAI)
- `context.length.exceeded` (generic)
- `too many tokens` (generic)
- Various other provider-specific patterns

Also detect silent overflow: if `stop_reason == Stop` and
`usage.input + usage.cache_read > context_window`, it's an overflow.

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

### 11.2 Unicode Sanitization

```rust
/// Remove unpaired Unicode surrogates that cause JSON serialization failures.
fn sanitize_surrogates(text: &str) -> String;
```

Note: Rust strings are valid UTF-8 by construction, so unpaired surrogates
cannot appear in `&str`. This is only needed when processing raw bytes or
data from external sources. The Rust implementation may be a no-op or a
validation check rather than a replacement.

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
   - Reasoning content field detection (reasoning_content, reasoning, reasoning_text)
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
