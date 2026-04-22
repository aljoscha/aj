# aj-models & Provider SDK Spec

This document specifies the target design for `aj-models`, `anthropic-sdk`, and `openai-sdk`.
It covers the unified message types, streaming protocol, provider implementations,
model registry, authentication (including OAuth), and cost tracking.

## Scope

- **Anthropic Messages API** (direct API key and OAuth/Claude Pro)
- **OpenAI Responses API** — primary OpenAI surface. Used for all
  native OpenAI models (direct API key and OAuth/ChatGPT Plus).
  Preserves encrypted reasoning across turns.
- **OpenAI Chat Completions API** — legacy surface. Kept for
  OpenAI-compatible third-party providers (Cerebras, Groq,
  OpenRouter, etc.) that only speak the Chat Completions shape.
  Reasoning is one-way here (see §7.2).
- Provider-agnostic types that enable cross-provider conversation replay
- OAuth flows for both Anthropic and OpenAI

---

## 1. Unified Message Types (`aj-models::types`)

All providers produce and consume these types. They are provider-independent.

### 1.1 Content Types

```rust
/// Text content block. Used by both `UserContent::Text` and
/// `AssistantContent::Text`. Note: `text_signature` is only meaningful
/// on assistant content — user messages always carry `None` here.
/// The type is shared to keep construction sites simple; the small
/// cost of an always-`None` field on user content is acceptable.
struct TextContent {
    text: String,
    /// Opaque signature carrying message metadata required for
    /// multi-turn replay against APIs that pair output messages with
    /// server-side item IDs. Populated by `openai-responses` as a
    /// JSON-encoded `TextSignatureV1` (see §7.3.4). Ignored by
    /// `anthropic-messages` and `openai-completions`. Always `None`
    /// on `UserContent::Text`.
    text_signature: Option<String>,
}

/// Extended thinking / reasoning content.
///
/// Valid field combinations (all others are malformed):
/// - **Signed:** `thinking` non-empty, `thinking_signature: Some`,
///   `redacted: false` — normal reasoning with a round-trippable
///   signature.
/// - **Redacted:** `thinking` empty, `thinking_signature: Some`
///   (carrying the encrypted payload), `redacted: true` — content
///   suppressed by safety filters; signature still round-trips.
/// - **Unsigned:** `thinking` non-empty, `thinking_signature: None`,
///   `redacted: false` — typically from an aborted stream. Cannot be
///   sent back as a thinking block; the per-provider serializers
///   (§6.2, §7.3.1) demote it to plain text when building the wire
///   request.
///
/// Invalid combinations — constructors must avoid emitting these:
/// - `redacted: true` with `thinking_signature: None` (nothing to
///   round-trip).
/// - `redacted: true` with non-empty `thinking` (contradicts the
///   "content was suppressed" semantics).
struct ThinkingContent {
    thinking: String,
    /// Opaque signature for multi-turn replay.
    /// Populated by `anthropic-messages` (base64 signature from
    /// `signature_delta`) and by `openai-responses` (JSON-serialized
    /// `ResponseInputItem::Reasoning`, carrying `id`, `summary`,
    /// `content`, and `encrypted_content` — see §7.3.3).
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
    /// Present when `stop_reason == Error` or `Aborted`. `None`
    /// otherwise. Shape defined in §1.3; retry semantics in §10.4.
    error: Option<AssistantError>,
    timestamp: i64,
}

struct ToolResultMessage {
    tool_call_id: String,
    tool_name: String,
    content: Vec<UserContent>,  // text and/or images
    /// Optional structured details preserved for UI/logs but never sent
    /// to the provider. Use this to attach rich metadata (diffs, file
    /// paths, exit codes, ...) that a tool produces for display,
    /// without forcing it through the model. Serialized with the
    /// thread; provider message conversion (§6.2, §7.2, §7.3.1)
    /// ignores it.
    details: Option<serde_json::Value>,
    is_error: bool,
    timestamp: i64,
}

enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}
```

### 1.3 Stop Reason, Usage & Error

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

/// Carried on `AssistantMessage.error` when `stop_reason == Error`
/// or `Aborted`. Providers classify upstream failures into one of the
/// categories below so callers can decide retry behaviour without
/// regex-matching the message string. Per-provider classification
/// tables live in §10.3; retry semantics in §10.4.
struct AssistantError {
    category: ErrorCategory,
    /// Human-readable failure message. Whatever the upstream surfaced,
    /// cleaned up (e.g. JSON-decoded `error.message`).
    message: String,
    /// Server-requested retry delay in milliseconds, populated from
    /// the `Retry-After` header or a body hint when present. `None`
    /// when the provider didn't specify a delay. Only meaningful for
    /// `RateLimit`, `Overloaded`, and `Transient` categories.
    retry_after_ms: Option<u64>,
    /// HTTP status from the originating response; `None` for
    /// transport-level failures, stream drops, and client aborts.
    http_status: Option<u16>,
}

enum ErrorCategory {
    /// 401 / 403 or OAuth refresh failure. Not retryable without
    /// re-authenticating.
    Auth,
    /// 429 rate-limit response. Retryable; honour `retry_after_ms`.
    RateLimit,
    /// Provider-overload response (Anthropic 529, OpenAI 503 overload
    /// body). Retryable with backoff.
    Overloaded,
    /// 5xx, transport error, or stream drop mid-response. Retryable,
    /// but note that partial output may already have been emitted
    /// (see §10.4).
    Transient,
    /// 400 whose message matches the context-overflow patterns in
    /// §10.5. Not retryable without reducing context.
    ContextOverflow,
    /// 400 that is not a context overflow (malformed request, unknown
    /// parameter, quota / billing, etc.). Not retryable.
    InvalidRequest,
    /// Safety filter refusal (Anthropic `refusal`, OpenAI
    /// `content_filter`, Responses `response.refusal`). Not retryable.
    ContentFilter,
    /// Client dropped the stream / cancelled the request.
    /// Pairs with `StopReason::Aborted`.
    Aborted,
    /// Catchall when the provider can't map the failure onto one of
    /// the above. Treat as not retryable by default.
    Unknown,
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

### 1.7 Agent-Level Message Extension

The wire types in §1.1–1.6 describe only what reaches the provider.
The agent transcript additionally holds entries that never leave the
process — notices emitted to the user, tool-result summaries rendered
in the TUI, token-usage markers, etc. These live one layer up and
are out of scope for `aj-models`.

Consumers that need them (today: `aj-agent`, which owns the existing
`UserOutput` type defined in `aj-ui`) define their own extension type
and project it onto wire `Message`s at each provider call boundary:

```rust
// In aj-agent (or wherever the transcript lives):
enum AgentMessage {
    Llm(Message),           // wire message from aj-models
    UserOutput(UserOutput), // UI-only entry
    // ... future: steering notes, branch markers, etc.
}

fn to_wire(msgs: &[AgentMessage]) -> Vec<Message> {
    msgs.iter().filter_map(|m| match m {
        AgentMessage::Llm(m) => Some(m.clone()),
        AgentMessage::UserOutput(_) => None, // dropped from wire
    }).collect()
}
```

`aj-models` never sees `AgentMessage`. Provider calls take
`Context { messages: Vec<Message> }`. The agent is responsible for
the `AgentMessage → Message` projection before each call and for
storing the full `AgentMessage` stream in the thread file (see §1.8).

### 1.8 Thread Persistence Format

Threads are stored as JSONL under
`~/.aj/threads/<encoded-cwd>/<thread_id>.jsonl`, where `<encoded-cwd>`
is the sanitized form of the project directory (matching the current
aj layout — e.g. `Dev-aj` for a project at `~/Dev/aj`). The on-disk
format is distinct from the wire types and has its own versioning.

**Line 1 is a header** carrying format metadata:

```rust
struct ThreadHeader {
    kind: String,                  // always "thread" (tag literal)
    format_version: u32,           // current: 1
    thread_id: String,
    created_at: i64,               // unix ms
    /// Project directory at thread creation. Required. Used to
    /// derive the thread's storage location (`~/.aj/threads/
    /// <encoded-cwd>/<thread_id>.jsonl`, same sharding pattern as
    /// today) and as a human-readable record on resume. Informational
    /// only on resume — the loader does not validate that the current
    /// process cwd matches this value; callers may resume a thread from
    /// any directory.
    cwd: String,
    parent_thread: Option<String>, // id of the thread this was forked from
}
```

**Every subsequent line is a `ThreadEntry`**, a tagged union:

```rust
struct ThreadEntry {
    timestamp: i64,
    #[serde(flatten)]
    kind: ThreadEntryKind,
}

enum ThreadEntryKind {
    /// An LLM message (user / assistant / tool-result).
    Message(Message),
    /// Agent-level entries that never go on the wire (see §1.7).
    /// Defined in the agent crate; aj-models treats the payload as
    /// opaque JSON so it can read threads produced by callers that
    /// extend the agent message type differently.
    Agent(serde_json::Value),
}
```

**Version handling.** The loader must check `format_version`. Unknown
values are rejected loudly (do not silently attempt to parse an
unrecognized format). The current version is `1`; bump it whenever
`ThreadEntry` or the embedded `Message` shape changes in a
non-backwards-compatible way.

**Pre-existing threads.** Adoption of this spec is a clean break.
The prior on-disk format is not supported by the v1 loader and there
is no migrator — threads directories from earlier builds must be
wiped (or moved aside) before running a v1-capable binary. Unknown
`format_version` values surface as a load error so stale files fail
fast instead of getting mis-parsed.

**What lives where.**

| Layer | Type | Crate | On wire? | Persisted? |
|---|---|---|---|---|
| Wire | `Message` (§1.2) | `aj-models` | Yes | Yes (as `ThreadEntryKind::Message`) |
| Agent | `AgentMessage` (§1.7) | `aj-agent` (or caller) | No | Yes (as `ThreadEntryKind::Agent`) |
| Thread file | `ThreadHeader` + `Vec<ThreadEntry>` | `aj-models` | No | Yes (this layer) |

`aj-models` owns `ThreadHeader`, `ThreadEntry`, `ThreadEntryKind`,
and the JSONL reader/writer. It does not know the shape of
`AgentMessage`; that field is a `serde_json::Value`. The agent crate
deserializes it into its own enum when loading and serializes its
enum into a Value when writing. This keeps `aj-models` agnostic to
callers that extend the agent message type.

### 1.9 Deliberately Out of Scope

Several content-block and entry kinds present in the current
in-repo SDK types (`ContentBlockParam` variants beyond the five
we keep) are deliberately excluded from the unified `Message` types
and the v1 thread format. None of them have an agent/tool/CLI caller
today. Any can be reintroduced later by adding a new
`AssistantContent` / `UserContent` variant and the matching provider
mapping, accompanied by a `format_version` bump.

| Omitted | Rationale | Re-add path |
|---|---|---|
| `CompactionBlock` (Anthropic server-side context compaction) | No agent caller; Anthropic streaming SDK parses them but we never consume | Add `AssistantContent::Compaction` + Anthropic provider mapping |
| `DocumentBlock` / PDF input | No agent/tool/CLI caller | Add `UserContent::Document` + Anthropic provider mapping |
| `ServerToolUseBlock` / `WebSearchToolResultBlock` / `CodeExecutionToolResultBlock` (+ bash / text-editor variants) | No agent wiring for server-side tools | Add variants per tool family when a tool-use path is wired |
| `MCPToolUseBlock` / `MCPToolResultBlock` | No MCP integration today | Same as above, gated on MCP wiring |
| `Citation` on text / tool-result | Plumbed through SDK, no UI rendering | Add `citations: Vec<Citation>` on `TextContent` + render path |
| Multi-block system prompt with per-block `cache_control` | `Context::system_prompt: Option<String>` sends one block. §6.2 auto-applies `cache_control` to the single block, which covers the common case. | Change to `system_prompt: Vec<SystemBlock>` with per-block `cache_breakpoint: bool`; update §6.2 to emit one `system` array entry per block and §7.2 / §7.3.1 to concatenate blocks with `\n\n` |

The following *thread-entry* kinds from other agent harnesses
(pi-mono, Claude Code session format, etc.) are also out of scope
for v1, but worth flagging as obvious future entry-kind candidates:

- `model_change` / `thinking_level_change` — record mid-thread
  config changes. Today these events are lost on replay (the
  `provider`/`api`/`model` fields on each `AssistantMessage` still
  carry per-turn provenance, which is what `transform_messages`
  uses, so correctness is preserved; only the UX of "why did the
  model change here" is lost).
- `compaction_summary` — a persisted summary produced when the
  agent compacts the earlier part of a thread to save tokens.
- `branch_summary` — a summary injected when a forked thread
  rejoins its parent.
- `label` / `thread_info` — user-facing metadata (bookmarks,
  display names).

These are out of scope because the agent doesn't produce any of them
yet. Calling them out here documents the absence rather than
committing to add them.

### 1.10 Round-trip Invariants

Round-trip correctness is a first-class guarantee of this abstraction.
For any assistant turn produced by a supported provider, the round-trip

```
provider SSE stream → unified AssistantMessage → provider request item
```

must preserve every piece of state that the provider needs to accept
the message in a subsequent request: signatures that validate, IDs
that pair, encrypted reasoning that decodes, content-block order that
matches the server's pairing expectations. "Accept" is defined as: the
provider treats the round-tripped request item as a valid prior
assistant turn in a multi-turn conversation, not that the JSON bytes
match verbatim.

This invariant is the load-bearing property that makes conversation
threads portable across aj restarts and cross-provider transforms
meaningful. The transform rules in §8 are defined on top of it — §8
says what changes when the target model differs; §1.10 says what must
*not* change when it doesn't.

**Per-provider preserved state** (a turn's multi-turn-significant
fields):

| Provider | Preserved | Mechanism |
|---|---|---|
| `anthropic-messages` | text blocks (text + order), thinking (text + signature), redacted_thinking (encrypted data), tool_use (id + name + arguments), tool_result (tool_use_id + content + is_error), stop_reason | Direct field mapping; `ThinkingContent.thinking_signature` carries the base64 signature or redacted payload |
| `openai-responses` | reasoning items whole (id, summary, content, encrypted_content), message items (id + phase), function_call composite (call_id + item_id), function_call_output call_id, the ordering between reasoning → message / function_call items | `ThinkingContent.thinking_signature` = serialized `ResponseInputItem::Reasoning`; `TextContent.text_signature` = `TextSignatureV1`; `ToolCall.id` = `"{call_id}\|{item_id}"` |
| `openai-completions` | text blocks, tool_calls (id + name + arguments), tool_result | Direct field mapping. **Reasoning is explicitly not preserved** — see §7.2; one-way is a provider-API limitation, not an aj-models correctness failure |

**Deliberately not preserved** (request-time hints, not message
state):

- `cache_control` markers on Anthropic requests — reapplied by the
  provider per §6.2 based on `StreamOptions.cache_retention`.
- Consecutive tool-result batching into one Anthropic user message —
  unified keeps tool results as separate `ToolResultMessage`s; the
  provider re-batches on the wire.
- `service_tier`, `prompt_cache_key`, `prompt_cache_retention` on
  Responses — per-request knobs driven by `StreamOptions`, not
  message state.
- `include`, `store`, `stream_options.include_usage` on OpenAI
  requests — per-request config.
- OAuth-stealth tool renaming — a wire transform driven by the
  active auth mode, not a stored property.

**Test coverage.** The invariant is enforced by the round-trip test
suite described in §12 Phase 4. That suite is the authoritative check
for §1.10 compliance; changes to provider mappings (§6.2, §7.2,
§7.3.1) that regress the invariant must be caught there before merge.

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
    /// Whether the model supports the `XHigh` reasoning level
    /// (Anthropic `output_config: {effort: "max"}` / OpenAI
    /// `reasoning_effort: "xhigh"`). Populated by the catalog
    /// (§3.4) — models.dev does not carry this flag, so it comes
    /// from the bundled overrides file. `false` for non-reasoning
    /// models.
    supports_xhigh: bool,
    /// Whether the model uses Anthropic's adaptive thinking API
    /// (`thinking: {type: "adaptive"}` + `output_config: {effort:
    /// ...}`) instead of the older budget-based thinking. Also
    /// governs whether the `interleaved-thinking-2025-05-14` beta
    /// header is sent (adaptive models reject / ignore it — see
    /// §6.1). `false` for non-Anthropic models and older Anthropic
    /// reasoning models.
    supports_adaptive_thinking: bool,
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

The registry holds all known models, organized by provider. It is
populated at startup from a JSON catalog: either the bundled seed
compiled into the binary via `include_str!`, or an optional user
cache at `~/.aj/models.json` written by the refresh CLI (see §3.4).
The registry has no runtime network dependency and loads fully
offline — the seed guarantees that.

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
/// Anthropic `output_config: {effort: "max"}` or OpenAI
/// `reasoning_effort: "xhigh"`). Thin accessor over
/// `ModelInfo.supports_xhigh`; the source of truth is the catalog
/// (seed + overrides, see §3.4). Wrapped as a function so provider
/// code reads `supports_xhigh(model)` regardless of future schema
/// changes (e.g. if the flag ever becomes derived).
fn supports_xhigh(model: &ModelInfo) -> bool;

/// Whether the model uses Anthropic's adaptive thinking API
/// (top-level `thinking: {type: "adaptive"}` + `output_config:
/// {effort: ...}`) rather than the older budget-based
/// `thinking: {type: "enabled", budget_tokens: N}`. Also governs
/// the `interleaved-thinking-2025-05-14` beta header — adaptive
/// models have interleaved thinking built in and do not need the
/// header (Opus 4.7 doesn't accept it; Opus 4.6 deprecates it;
/// Sonnet 4.6 ignores it redundantly). Thin accessor over
/// `ModelInfo.supports_adaptive_thinking`.
fn supports_adaptive_thinking(model: &ModelInfo) -> bool;
```

Additional capability probes may be added as new tiered features
appear (e.g. `supports_images`, `supports_cache_1h`). Each probe
returns a boolean derived from the static catalog entry — no I/O.

### 3.4 Model Catalog Sources

The model catalog is **data, not code.** It lives in JSON files, not
generated Rust. Two sources feed the registry, in priority order:

1. **User cache** (`~/.aj/models.json`): if present and parseable,
   this is the authoritative catalog for the current run. Populated
   by `aj models update` (§3.4.5).
2. **Bundled seed** (`src/aj-models/data/models.json`, included via
   `include_str!`): compile-time fallback. Always present; guarantees
   the registry loads on first run, offline, or when models.dev is
   unreachable. We update the seed periodically by running the
   refresh command and copying the resulting cache over the seed
   before committing.

If the user cache exists but fails to parse, the registry logs a
warning and falls back to the seed. A missing user cache is not an
error — the seed is used silently.

**Overrides** (`src/aj-models/data/overrides.json`, bundled) are
applied on top of whichever catalog loaded. They correct known
inaccuracies in upstream data (e.g. wrong cache pricing), supply
fields models.dev doesn't carry (e.g. `supports_xhigh`,
`supports_adaptive_thinking`), and add brand-new models not yet
indexed upstream. Overrides run on every load — seed and user cache
alike — so a fresh refresh never wipes our authored corrections.

#### 3.4.1 Catalog Schema

Seed and user cache share one JSON schema:

```json
{
  "updated_at": 1745280000000,
  "source": "models.dev",
  "models": [
    {
      "id": "claude-sonnet-4-6-20260101",
      "name": "Claude Sonnet 4.6",
      "api": "anthropic-messages",
      "provider": "anthropic",
      "base_url": "https://api.anthropic.com",
      "reasoning": true,
      "supports_xhigh": false,
      "supports_adaptive_thinking": true,
      "input": ["text", "image"],
      "cost": {
        "input": 3.0,
        "output": 15.0,
        "cache_read": 0.3,
        "cache_write": 3.75
      },
      "context_window": 200000,
      "max_tokens": 64000,
      "headers": null
    }
  ]
}
```

`updated_at` is unix ms at the time the refresh command wrote the
file (for the seed: the time the maintainer ran refresh before
committing). It drives the staleness warning in §3.4.6.

#### 3.4.2 Fetching from models.dev

`aj models update` fetches `https://models.dev/api.json` natively
(reqwest + serde — no Python dependency), filters to tool-calling
models on `anthropic` + `openai` providers, maps fields per the
table below, fills provider-specific fixed values (§3.4.3), applies
overrides (§3.4.4), and writes the result to `~/.aj/models.json`.

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

`supports_xhigh` and `supports_adaptive_thinking` are not in
models.dev; they come from overrides (§3.4.4).

On fetch failure (network error, non-200, parse failure), the
command exits non-zero and leaves `~/.aj/models.json` untouched —
a broken fetch never bricks the registry.

#### 3.4.3 Provider-Specific API Mapping

For each provider, fixed values:

| Provider | `api` | `base_url` |
|---|---|---|
| `anthropic` | `"anthropic-messages"` | `"https://api.anthropic.com"` |
| `openai` | `"openai-responses"` | `"https://api.openai.com/v1"` |

**One api per `(provider, id)`, no duplication.** The catalog
contains exactly one `api` string per `(provider, id)` pair. Users
do not pick between Chat Completions and Responses for a native
OpenAI model; the provider's preferred API is hard-coded in the
catalog. Native `openai` models use `"openai-responses"`.

If future third-party OpenAI-compatible providers (Cerebras, Groq,
OpenRouter, etc.) are added, those get `"openai-completions"`
because they only speak the Chat Completions shape.

#### 3.4.4 Overrides Format

Overrides live in `src/aj-models/data/overrides.json` and are
applied in order at load time:

```json
{
  "overrides": [
    {
      "target": {"provider": "anthropic", "id": "claude-opus-4-7-20260115"},
      "patch": {
        "supports_xhigh": true,
        "supports_adaptive_thinking": true,
        "cost": {"cache_read": 0.30}
      },
      "reason": "xhigh/adaptive flags aren't in models.dev; cache_read corrected from upstream typo"
    }
  ]
}
```

The `patch` object is shallow-merged onto the matching model entry.
Nested objects (`cost`, `input`) are replaced whole, not deep-merged
— predictable wins over clever. Each entry must carry a `reason`
string for reviewer context.

Overrides apply to both the seed and the user cache on every load,
so our authored corrections survive refreshes.

#### 3.4.5 Refresh CLI

```
aj models update
```

Fetches models.dev, applies overrides, writes `~/.aj/models.json`.
Prints a short diff summary on success (`added X models, removed Y,
price changes on Z`) and exits 0. Fetch failures exit non-zero and
leave the existing cache intact.

#### 3.4.6 Staleness Warning

On registry load, if the active catalog's `updated_at` is more than
90 days old, the registry logs a one-line warning recommending
`aj models update`. The 90-day threshold is a compromise — pricing
changes on the order of months, new models ship every few weeks; a
tighter threshold would warn too often on stable catalogs. Tunable.

No automatic refresh is performed — refresh is always explicit.

#### 3.4.7 Filtering Rules

A catalog entry is included only if:
- `tool_call == true` on the upstream source (we need function calling)
- The provider is `"anthropic"` or `"openai"`

Filtering runs during fetch (§3.4.2); the seed and user cache
already contain only filtered entries, so the load path does not
re-filter.

---

## 4. Stream Options

Options passed to any streaming call:

```rust
struct StreamOptions {
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    api_key: Option<String>,
    /// Prompt cache retention preference.
    /// - Anthropic: `None` sends no `cache_control`; `Short` sends
    ///   `cache_control: {type: "ephemeral"}` (5-minute default TTL,
    ///   no explicit `ttl` field); `Long` adds `ttl: "1h"`.
    /// - OpenAI Responses: controls `prompt_cache_retention`
    ///   (`"24h"` on Long, omitted otherwise). Caching itself is
    ///   automatic — this field only adjusts retention.
    /// - `openai-completions` ignores this; its cache is automatic
    ///   and not tunable.
    cache_retention: CacheRetention,
    /// Stable identifier for a multi-turn conversation. Callers
    /// should pass the same value across every request that belongs
    /// to one conversation, not rotate it per request — the whole
    /// point is session affinity.
    ///
    /// Provider-specific uses (all mean "route this request to the
    /// worker that already has state for this session"):
    /// - `openai-responses`: passed as `prompt_cache_key` when
    ///   `cache_retention != None` (see §7.3.2), and also forwarded
    ///   as `session_id` and `x-client-request-id` request headers
    ///   (see §7.3). The headers are only sent to
    ///   `api.openai.com` — Azure and other deployments may reject
    ///   unknown headers.
    /// - `anthropic-messages` and `openai-completions`: currently
    ///   unused. Reserved for future session-caching hooks.
    session_id: Option<String>,
    /// Extra HTTP headers merged with provider defaults.
    headers: Option<HashMap<String, String>>,
    /// Metadata fields (e.g. Anthropic user_id for rate limiting).
    metadata: Option<HashMap<String, serde_json::Value>>,
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

#[derive(Default)]
enum CacheRetention {
    None,
    #[default]
    Short,  // Anthropic "5m", Responses cache without retention hint
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

**Retry:** there is intentionally no retry knob on `StreamOptions`.
Providers never auto-retry failed requests; they classify the error
per §10.3 and surface an `Error` / `Aborted` final `AssistantMessage`
immediately. The caller (today: `aj-agent`) owns the retry loop and
respects `AssistantError.retry_after_ms`. The only exception is the
one-shot OAuth refresh-and-retry path in §9 (transparent at the auth
layer, invisible to callers).

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
| API Key   | `x-api-key: <key>` | `anthropic-beta: fine-grained-tool-streaming-2025-05-14` (add `,interleaved-thinking-2025-05-14` when reasoning is enabled and `!supports_adaptive_thinking(model)`) |
| OAuth     | `Authorization: Bearer <token>` | `anthropic-beta: claude-code-20250219,oauth-2025-04-20,fine-grained-tool-streaming-2025-05-14` (add `,interleaved-thinking-2025-05-14` under the same condition as API Key mode), `user-agent: claude-cli/<ver>`, `x-app: cli` |

Adaptive-thinking models (see `supports_adaptive_thinking` in §3.3.1)
have interleaved thinking built in and do not accept the beta header:
Opus 4.7 doesn't accept it, Opus 4.6 deprecates it, Sonnet 4.6 ignores
it redundantly. The direct Anthropic API silently ignores the header
when unnecessary — Bedrock/Vertex proxies reject it on unsupported
models, so the probe matters if we ever add those providers.

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
- Thinking blocks with a signature → `{type: "thinking", thinking: "<text>", signature: "<thinking_signature>"}`
- Redacted thinking blocks → `{type: "redacted_thinking", data: "<thinking_signature>"}`
- Thinking blocks without signatures (from aborted streams) → plain text blocks
- Images as base64 with media type

**Default `max_tokens`:** If `StreamOptions.max_tokens` is `None`, the
provider sends `model.max_tokens / 3` as a reasonable default.

**Prompt caching:** driven by `StreamOptions.cache_retention`.
- `None`: send no `cache_control` markers. The request is processed
  uncached; `cache_creation_input_tokens` and `cache_read_input_tokens`
  will both be 0.
- `Short` (default): add `cache_control: {type: "ephemeral"}` to the
  system prompt's last block and to the last user message's last
  content block. Default 5-minute TTL.
- `Long`: same as `Short` but with `ttl: "1h"` on each `cache_control`
  marker, and only when the base URL is `api.anthropic.com`
  (Bedrock/Vertex proxies may not support the TTL field — they get
  `Short` behavior).

**Note on Anthropic's minimum cacheable prefix:** 4096 tokens on
Opus/Haiku 4.x, 2048 on Sonnet 4.6. Shorter prefixes silently don't
cache. We don't gate on this — the caller chooses when to enable
caching, and short prompts simply produce zero cache activity.

**Thinking/reasoning configuration:** gated on `supports_adaptive_thinking(model)` (see §3.3.1).
- Adaptive models (`supports_adaptive_thinking(model) == true`): set top-level `thinking: {type: "adaptive"}` and top-level `output_config: {effort: "low"|"medium"|"high"|"max"}` (both are sibling request fields, not nested).
- Non-adaptive reasoning models (`model.reasoning == true && !supports_adaptive_thinking(model)`): use `thinking: {type: "enabled", budget_tokens: N}`.
- Non-reasoning or thinking disabled by caller: `thinking: {type: "disabled"}`.
- ThinkingLevel mapping for adaptive: Minimal→`"low"`, Low→`"low"`, Medium→`"medium"`, High→`"high"`, XHigh→`"max"` when `supports_xhigh(model)`, otherwise falls back to `"high"`.
- ThinkingLevel mapping for budget-based: Minimal→1024, Low→2048, Medium→8192, High→16384 (XHigh falls back to High).

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

**Responses-specific options:** `StreamOptions.service_tier` and
`StreamOptions.reasoning_summary` are ignored by this provider.
Anthropic's `service_tier` is in beta and its cost model (reservation-
based) doesn't match the simple multiplier used for Responses;
mapping is deferred until the beta stabilizes. Anthropic's adaptive
thinking has no summary-verbosity concept.

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
  - **Consequence:** reasoning is one-way on Chat Completions. The
    provider captures `delta.reasoning_content` as `ThinkingContent`
    during streaming (so the UI can render it live), but on the next
    turn that thinking block is dropped per the outbound rule. Multi-
    turn reasoning continuity is not available on this API — callers
    who need it must use `openai-responses` instead.
  - Empty assistant messages (from aborted streams) are dropped
- `ToolResultMessage` → `{role: "tool", content: string, tool_call_id: string}`
  - Images from tool results: inject a subsequent `user` message with `image_url` parts

**Store:** explicitly set `store: false` on requests. The Chat
Completions API defaults to `false`, but sending it explicitly ensures
conversations are never stored server-side even if the default changes.

**Reasoning effort:**
- Set `reasoning_effort` on the request for models that support it
- ThinkingLevel mapping: Minimal→"minimal", Low→"low", Medium→"medium", High→"high", XHigh→"xhigh" (GPT-5.2+ only; falls back to "high" on other models)
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
- `content_filter` → `Error`, `error: { category: ContentFilter, message: "Provider finish_reason: content_filter", .. }`
- `network_error` → `Error`, `error: { category: Transient, message: "Provider finish_reason: network_error", .. }`
- any other value → `Error`, `error: { category: Unknown, message: "Provider finish_reason: {value}", .. }`

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
- `AssistantMessage` → expanded into multiple input items, **in the
  order the content blocks appear in `AssistantContent`** (order is
  load-bearing: the server pairs reasoning items with the following
  message or function_call items):
  - **Thinking blocks with signature**: deserialize `thinking_signature` back
    into a full `reasoning` input item (the signature contains the serialized
    `ResponseReasoningItem` JSON — see §7.3.3)
  - **Text blocks**: `{type: "message", role: "assistant", id: <msg_id>,
    content: [{type: "output_text", text: "..."}], status: "completed"}`
    where `msg_id` is extracted from `text_signature` (see §7.3.4)
  - **Tool calls**: `{type: "function_call", id: <item_id>, call_id: <call_id>,
    name: "...", arguments: "..."}` — the composite ID stored in `ToolCall.id`
    is split on `|` to recover `call_id` and `item_id` (see §7.3.5)
  - **Cross-model replay:** reasoning items (carried in
    `thinking_signature`) are model-bound — the Responses API returns
    `invalid_encrypted_content` when an item is sent to a different
    model. Dropping on model change is handled by the generic rule in
    §8.1 rule 2. Additionally, for **tool calls** from a different
    model within the same provider+api, omit the `id` field entirely
    from the outbound `function_call` input item (keep `call_id`) to
    avoid server-side pairing validation between `fc_xxx` IDs and
    `rs_xxx` reasoning items.
  - Thinking blocks without signatures are **demoted to plain text**
    per §8.1 rule 2 (same-model aborted-stream residue keeps its
    content in the thread, reclassified as assistant text).
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
  reasoning: {effort: <level>, summary: <reasoning_summary or "auto">},  // only on reasoning-capable models
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

**Prompt caching:** caching is automatic on the Responses API — no
key required. The fields below are routing/retention hints that
improve hit rate and extend TTL; they never *enable* caching.
- `prompt_cache_key`: set to `StreamOptions.session_id` when
  `cache_retention != None` and a `session_id` is present. Omitted
  otherwise. Acts as a routing hint that keeps requests with the
  same session sticky to a worker that already has the KV state.
- `prompt_cache_retention`: set to `"24h"` when
  `cache_retention == Long` and the base URL is `api.openai.com`.
  Omitted otherwise — default retention is `in_memory` (~5–10 min,
  up to 1h on idle).

**Store:** hardcoded to `false`. Server-side conversation storage is not
used currently. If enabled in the future, it would allow use of
`previous_response_id` for server-side conversation chaining (the API
resumes from a prior response, avoiding resending the full history).
Both are Responses-specific concerns and do not belong in the base
`StreamOptions`.

**Reasoning configuration:**
- Non-reasoning models (`model.reasoning == false`): **omit the
  `reasoning` parameter entirely.** Sending `reasoning` to
  non-reasoning models (e.g. `gpt-4o`, `gpt-4.1`) is rejected by the
  API.
- Reasoning-capable model, reasoning requested: `reasoning: {effort:
  <mapped_level>, summary: <reasoning_summary or "auto">}` and
  `include: ["reasoning.encrypted_content"]` to receive encrypted
  reasoning for multi-turn continuity.
- Reasoning-capable model, reasoning disabled by caller:
  `reasoning: {effort: "none"}` (without `include`). The model still
  accepts the parameter; the effort level explicitly opts out.
- ThinkingLevel mapping: same as §7.2 (Minimal→"minimal", ..., XHigh→"xhigh")

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

**The `include` requirement.** Round-trip requires `include:
["reasoning.encrypted_content"]` on the request (§7.3.2). Without
it, the server returns reasoning items without an `encrypted_content`
field — i.e. only `id` and `summary`. Sending such an item back on
the next turn carries no real state, so the chain-of-thought is
lost even if the server accepts the request. This is why the
reasoning-enabled branch of §7.3.2 always sets `include`, and it's
a prerequisite for the §1.10 round-trip invariant on this provider.
pi-mono does the same thing unconditionally.

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

**Sanitization applies to both halves of the composite.** Responses
enforces the `[a-zA-Z0-9_-]` character class and a ≤64 char length
on IDs. When building a `function_call` input item for Responses,
apply sanitize + truncate (character class + length rules from §8.1
rule 3) to both the `call_id` and the `item_id` before any `fc_`
substitution. The Responses API does *not* enforce a `call_` prefix
on `call_id` (verified empirically — foreign call_ids like
Anthropic's `toolu_xxx` are accepted as-is), so no prefix rewrite is
needed on that half.

#### 7.3.6 Stream Event Mapping

| Responses SSE | Unified Event |
|---|---|
| `response.created` | capture `response_id` |
| `response.output_item.added` (reasoning) | `ThinkingStart` |
| `response.output_item.added` (message) | `TextStart` |
| `response.output_item.added` (function_call) | `ToolCallStart` (with composite ID) |
| `response.content_part.added` | track content part within message item (accept `output_text` and `refusal` only) |
| `response.reasoning_summary_part.added` | on every part after the first, emit a `ThinkingDelta` with `"\n\n"` to separate parts. Do not emit on the first part. |
| `response.reasoning_summary_text.delta` | `ThinkingDelta` |
| `response.reasoning_summary_part.done` | no-op (part boundary — the separator is emitted on the next part's `added` event, so the stream never ends with trailing `"\n\n"`) |
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
separated by `\n\n`, emitted on each `part.added` event except the
first, so the accumulated `thinking` field never ends with trailing
whitespace. The full `ResponseReasoningItem` (with summary array) is
captured on `response.output_item.done`.

#### 7.3.7 Usage Parsing

Usage arrives in `response.completed` → `response.usage`:
- `input_tokens` includes cached tokens; subtract
  `input_tokens_details.cached_tokens` for non-cached input
- `output_tokens` maps directly to `usage.output`
- `total_tokens` maps to `usage.total_tokens`
- `cache_write` is 0 (Responses API does not report cache writes separately).
  `usage.cost.cache_write` will therefore always be 0 on this
  provider regardless of `ModelCost.cache_write`; cache-write cost
  is folded into `input` by OpenAI's pricing model.

#### 7.3.8 Stop Reason Mapping

The Responses API uses `response.status` instead of `finish_reason`:

| Response status | Stop Reason |
|---|---|
| `completed` | `Stop` (override to `ToolUse` if content contains tool calls) |
| `incomplete` | depends on `incomplete_details.reason` — see below |
| `failed` | `Error` |
| `cancelled` | `Error` |
| `in_progress` | `Stop` (should not appear on a finished response; handle defensively) |
| `queued` | `Stop` (should not appear on a finished response; handle defensively) |

**`incomplete` subcases:** inspect `response.incomplete_details.reason`:
- `"max_output_tokens"` / `"length"` → `Length`
- `"content_filter"` → `Error` (error message: `"Incomplete: content_filter"`)
- `"max_tool_calls"` → `ToolUse` (model hit the tool-call cap but was still producing tool calls)
- any other value or missing → `Length` (safe default — the response was cut off for some resource-limit reason)

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
   Pass-through here governs only the transform layer; the per-provider
   serializers (§6.2, §7.3.1) may still demote unsigned thinking blocks
   to plain text when building the wire request, since there is nothing
   to round-trip in that case.
2. **Different model** (different provider, different api, or different model id):
   - Redacted thinking blocks: **drop** (encrypted for the original model).
   - Thinking blocks with signatures: **drop the signature
     unconditionally.** Any model change invalidates it: Anthropic
     signatures are cryptographically bound to the producing model,
     and `openai-responses` returns `invalid_encrypted_content` when
     a reasoning item crosses model boundaries. If `thinking` is
     non-empty, convert the visible text to a plain-text assistant
     block (same treatment as the "non-empty text, no signature"
     bullet below); if `thinking` is empty, drop the block entirely.
     Redacted thinking blocks (see the preceding bullet) are a
     separate case — they are empty by invariant, so this rule does
     not apply to them. We do not carve out a per-provider exception
     for the signature drop — if a future provider documents cross-
     model signature compatibility, revisit this rule then.
   - Thinking blocks with empty text and no signature: **drop**.
   - Thinking blocks with non-empty text but no signature: **convert
     to plain text.** This shape typically arises from aborted streams
     where the UI already surfaced the text; demoting preserves thread
     continuity at the cost of reclassifying reasoning as speech.
   - Text blocks: strip `text_signature` (provider-specific).
   - Tool calls: normalize IDs to target provider's format (see rule 3).
   - **`openai-responses`:** additional cross-model tool-call
     constraints apply during message conversion — see §7.3.1
     "Cross-model replay".

3. **Tool call ID normalization.** Apply in order for each
   `ToolCall.id` whose source assistant message is from a different
   model (rule 2 cases):

   - For `openai-completions` targets: if the ID contains `|` (a
     composite produced by §7.3.5 for Responses sources), split on
     `|` and keep only the first part (the `call_id`). The `item_id`
     portion is not representable in Completions and is discarded.
   - For `openai-responses` targets: the ID is a `{call_id}|{item_id}`
     composite. Apply the sanitize + truncate steps below to each
     half independently, then apply the `fc_<short_hash>` substitution
     to the `item_id` half for foreign-origin tool calls per §7.3.5.
     The `call_id` half needs no prefix rewrite.
   - Sanitize: replace any character outside `[a-zA-Z0-9_-]` with `_`.
   - Truncate to the target's length limit:
     - `anthropic-messages`: ≤64 chars.
     - `openai-completions`: ≤40 chars.
     - `openai-responses`: ≤64 chars per half.

4. **Orphaned tool calls:** If an assistant message has tool calls but the
   conversation has no corresponding `ToolResultMessage`, insert a synthetic
   error tool result: `{content: "No result provided", is_error: true}`.

5. **Errored/aborted assistant messages (pass 2):** Skip entirely
   (stopReason == Error or Aborted), along with any `ToolResultMessage`s
   that referenced their tool calls. These are incomplete turns that
   shouldn't be replayed. This is applied in pass 2, not pass 1,
   because skipping the assistant message must also remove its
   associated tool results.

### 8.2 Capability Downgrade

Separate from cross-provider transformation, the target model may
lack a capability the source content uses. Apply these downgrades
after §8.1 runs.

**Images on non-vision models.** If
`!target_model.input.contains(&InputModality::Image)`, replace every
`UserContent::Image` with a `UserContent::Text` placeholder:

- In a `UserMessage`: `"(image omitted: model does not support images)"`
- In a `ToolResultMessage`: `"(tool image omitted: model does not support images)"`

When multiple images appear consecutively in the same content array,
emit a single placeholder (deduplicate runs) so the transcript
doesn't balloon with identical markers. The placeholder strings are
fixed — callers that inspect them downstream should match against
these exact values.

No other downgrades are defined today. As new modalities or
capabilities surface (audio input, document input, tool-result
variants that only some providers accept), add entries here with the
same shape: a predicate against `target_model`, a replacement rule,
and any dedup semantics.

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
2. Environment variables — provider-specific (see §9.5). Env vars
   win over `auth.json` so a developer can dev-override stored
   credentials for a one-off run without editing the file.
3. API key from `auth.json`
4. OAuth token from `auth.json` (auto-refreshed if expired)

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

| Provider | Environment Variables (first match wins) |
|----------|---------------------|
| anthropic | `ANTHROPIC_OAUTH_TOKEN`, then `ANTHROPIC_API_KEY` |
| openai | `OPENAI_API_KEY` |

Per §9.1, any of these env vars overrides credentials stored in
`auth.json`.

---

## 10. Error Model

This section defines how provider failures are surfaced to callers
and how they should be handled. Providers never retry; the caller
owns the retry loop (see §4 "Retry" and §10.4 below). The shape
callers match against is `AssistantError` from §1.3.

### 10.1 AssistantMessage error shape

Recap from §1.2 and §1.3: a failed or aborted turn terminates with:
- `stop_reason == Error` or `Aborted`
- `error == Some(AssistantError { category, message, retry_after_ms, http_status })`
- The preceding `content` vector may still hold partial output that
  was streamed before the failure. Callers decide whether to discard
  it or present the partial to the user; see §10.4 for the
  mid-stream retry caveat.
- `usage` reflects whatever the provider reported before the
  failure — partial counts if usage was streamed, zero if the
  failure happened before any usage event arrived. Cost fields are
  computed from these partial counts via §3.3 regardless.

### 10.2 Error categories

See `ErrorCategory` in §1.3 for the enum. Category choice is a
contract: callers key retry behaviour off `category`, not the
free-form `message` text. Categories are stable; messages are not.

- **Retryable:** `RateLimit`, `Overloaded`, `Transient`.
- **Not retryable:** `Auth`, `ContextOverflow`, `InvalidRequest`,
  `ContentFilter`, `Aborted`.
- **`Unknown`:** not retryable by default. The agent may choose to
  retry with a small bounded budget if it wants to be optimistic,
  but the provider makes no guarantees about idempotency.

### 10.3 Per-provider error mapping

Providers translate upstream responses into `AssistantError`
according to the tables below. Where the response body carries a
typed tag, use the tag; otherwise classify by HTTP status plus
message patterns. Whenever a fallback match (§10.5 regex net) fires
on a category other than `ContextOverflow`, that's a signal the
table below should be updated.

**Anthropic (`anthropic-messages`).** The API returns a typed error
envelope `{ type: "error", error: { type, message } }`:

| Anthropic `error.type` | HTTP | Category |
|---|---|---|
| `authentication_error` | 401 | `Auth` |
| `permission_error` | 403 | `Auth` |
| `not_found_error` | 404 | `InvalidRequest` |
| `invalid_request_error` (matches §10.5 patterns) | 400 | `ContextOverflow` |
| `invalid_request_error` (other) | 400 | `InvalidRequest` |
| `request_too_large` | 413 | `ContextOverflow` |
| `billing_error` | 402 | `InvalidRequest` |
| `rate_limit_error` | 429 | `RateLimit` |
| `overloaded_error` | 529 | `Overloaded` |
| `api_error` | 5xx | `Transient` |
| `timeout_error` | 504 | `Transient` |

Stop-reason-driven: `refusal` / `sensitive` stop reasons (§6.2)
translate to `ContentFilter`.

`retry_after_ms`: populated from the `retry-after` response header
when present — integer seconds → ms, HTTP-date format also accepted.
No body-level hint on Anthropic.

**OpenAI Chat Completions (`openai-completions`).** Error body is
`{ error: { code, message, type, param } }`:

| OpenAI `error.code` / `error.type` | HTTP | Category |
|---|---|---|
| `invalid_api_key`, auth-looking `invalid_request_error` | 401 | `Auth` |
| forbidden | 403 | `Auth` |
| `context_length_exceeded` | 400 | `ContextOverflow` |
| `invalid_request_error` (matches §10.5 patterns) | 400 | `ContextOverflow` |
| `invalid_request_error` (other) | 400 | `InvalidRequest` |
| `insufficient_quota` | 429 | `InvalidRequest` (billing, not retryable) |
| `rate_limit_exceeded` | 429 | `RateLimit` |
| 5xx server errors | 5xx | `Transient` |
| `finish_reason == content_filter` | 200 | `ContentFilter` |
| `finish_reason == network_error` | 200 | `Transient` |

`retry_after_ms`: populated from `retry-after` response header.

**OpenAI Responses (`openai-responses`).** Initial HTTP errors
follow the same mapping as Chat Completions. Mid-stream failures
arrive as `response.failed` or top-level `error` SSE events with
`{code, message}`:

| Responses failure | Category |
|---|---|
| `response.status == failed`, `error.code` matches a known HTTP-level code above | as per that code |
| `response.status == failed`, unknown code | `Unknown` |
| `response.status == cancelled` | `Aborted` |
| `response.status == incomplete`, `incomplete_details.reason == content_filter` | `ContentFilter` |
| `response.refusal` delta terminates response | `ContentFilter` |
| SSE `error` event | `Unknown` (finer if `code` maps to a known category) |

**Transport and stream failures (any api).**

| Condition | Category |
|---|---|
| Connection reset / DNS failure / TLS failure before response | `Transient` |
| Stream ends before `Done` or `Error` | `Transient` (provider synthesizes the `Error` event per §2) |
| Caller drops the stream | `Aborted` |

### 10.4 Retry policy

**Providers do not retry.** They surface the error as described in
§10.1 and `Provider::stream` returns with the final `Error` /
`Aborted` event on the stream.

The only exception is OAuth token refresh (see §9): if the auth
layer hands out an access token and the subsequent HTTP call returns
401, the client performs **exactly one** transparent refresh-and-retry
before classifying as `Auth`. This is a narrow guard against
credential-rotation races, not a general retry policy.

Callers implementing retry (today: `aj-agent`) should:

1. Read `error.category`. If it's not in `{ RateLimit, Overloaded,
   Transient }`, do not retry.
2. Compute a backoff delay:
   - If `retry_after_ms` is set, use it.
   - Otherwise, exponential backoff starting at ~1s with full jitter.
3. Before sleeping, verify the delay doesn't exceed a caller-chosen
   ceiling (`aj-agent` uses a 60s default). If it would, surface
   the error to the user instead of silently waiting.
4. Bound the total number of retry attempts (e.g. 3 for `Transient`,
   5 for `RateLimit`). Past the budget, surface the error.
5. For `Transient` errors that occur *mid-stream*, remember that
   partial output may already have been displayed. Do not silently
   re-run unless the caller can either suppress the earlier partial
   or tolerate duplication.

Cross-model or cross-provider retry (e.g. on `ContextOverflow`,
switch to a longer-context model) is a higher-level strategy that
belongs to the agent, not the retry layer.

### 10.5 Context overflow detection

```rust
/// Check if an AssistantMessage represents a context overflow error.
fn is_context_overflow(message: &AssistantMessage, context_window: Option<u64>) -> bool;
```

**Primary path:** if `error.category == ContextOverflow`, return
`true`. This is the canonical signal — providers classify
overflow-shaped errors into this category per §10.3.

**Fallback regex matching** exists for defensive handling of errors
whose category ended up as `InvalidRequest` or `Unknown` because the
upstream response didn't match a known code (proxy-reshaped errors,
upstream message churn). When `error.category` is `InvalidRequest`
or `Unknown`, apply the patterns below against `error.message`.

**Non-overflow exclusion patterns** (skip overflow detection if matched):
- `/rate limit/i`
- `/too many requests/i`

**Overflow patterns** (match after the exclusion check passes):

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

When a fallback regex fires, `is_context_overflow` returns `true`
and the provider's §10.3 table should be updated so future requests
get `category: ContextOverflow` directly, without relying on the
regex net.

**Silent overflow.** Some providers accept overflowing requests
without error. Detect this when `stop_reason == Stop` and
`usage.input + usage.cache_read > context_window` (requires the
caller to pass the model's context window).

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
   `ModelInfo` (including `supports_xhigh` and `supports_adaptive_thinking`
   fields), `ModelCost`, `InputModality`, `ModelRegistry` from §3.
   Implement `calculate_cost` and the capability-probe accessors from §3.3.1.
   The registry loads its catalog from (a) the user cache at
   `~/.aj/models.json` if present and parseable, otherwise (b) the bundled
   seed at `src/aj-models/data/models.json` via `include_str!`. Overrides
   from `src/aj-models/data/overrides.json` are applied on top. Emit the
   staleness warning per §3.4.6.

3b. **Build the catalog refresh command** (`aj models update`): Native
    Rust (reqwest + serde), no Python. Fetches models.dev, filters to
    Anthropic + OpenAI tool-capable models, fills provider-specific
    fixed values (§3.4.3), applies overrides (§3.4.4), writes
    `~/.aj/models.json`. On fetch failure: non-zero exit, leave the
    existing cache intact. Run it once against a live models.dev,
    apply manual overrides for `supports_xhigh` /
    `supports_adaptive_thinking`, and commit the resulting JSON as
    the initial seed (`src/aj-models/data/models.json`) and the
    initial overrides file (`src/aj-models/data/overrides.json`).

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

11. **Error classification & overflow detection** (`aj-models::errors`):
    Implement `AssistantError` / `ErrorCategory` from §1.3, the
    per-provider classification tables from §10.3 (called from the
    provider SDKs when building the terminal error message), and
    `is_context_overflow` from §10.5 (primary path via
    `error.category`, regex fallback for `InvalidRequest` / `Unknown`).

11b. **Round-trip test suite** (`src/aj-models/tests/roundtrip/`):
    Enforces the §1.10 invariant. Fixtures live under
    `fixtures/<api>/<scenario>.{sse,request.json}` — captured SSE
    streams and hand-crafted golden request bodies. Three test
    shapes per provider:

    - **Parse test**: SSE fixture → unified `AssistantMessage`.
      Structural assert (snapshot or hand-written expected). One
      scenario per `AssistantContent` variant the provider emits —
      text-only, thinking+text, tool_call, redacted_thinking
      (Anthropic), reasoning+message+function_call (Responses).
    - **Serialize test**: hand-crafted `AssistantMessage` fixture →
      provider request item JSON. Golden-file compare.
    - **Semantic round-trip test**: captured-and-parsed
      `AssistantMessage` → serialize as request item → re-parse that
      request item back to `AssistantMessage` via a new
      `parse_assistant_request_item` helper per provider (symmetric
      to the streaming parser; request and response share content-
      block shapes on each provider, so this is cheap) → assert
      field-equal to the original modulo the "deliberately not
      preserved" list in §1.10.

    Plus one **cross-provider transform test** per direction
    (Anthropic ↔ Responses, Anthropic ↔ Completions): feed a multi-
    turn history through `transform_messages(target=X)` and assert
    the §8.1 rules fire correctly — signatures dropped on model
    change, visible reasoning text demoted to plain text per rule 2,
    tool-IDs normalized per rule 3, orphan results inserted per
    rule 4, errored turns skipped per rule 5.

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
