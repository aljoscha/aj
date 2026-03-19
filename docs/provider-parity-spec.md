# Provider SDK & Model Layer Parity Spec

This document specifies the gaps between our current provider SDK + `aj-models`
implementation and the reference implementation (pi-ai). It covers Anthropic and
OpenAI only. Each section describes what pi-ai does, what we have today, and
what needs to change.

---

## 1. Unified Message & Content Types (`aj-models`)

### What pi-ai does

Three message types with clear roles:

```
UserMessage     { role: "user",      content: string | (TextContent | ImageContent)[] }
AssistantMessage{ role: "assistant", content: (TextContent | ThinkingContent | ToolCall)[] }
ToolResultMessage{ role: "toolResult", toolCallId, toolName, content: (TextContent | ImageContent)[], isError }
```

Content block types are intentionally minimal:
- `TextContent` — text + optional textSignature
- `ThinkingContent` — thinking text + thinkingSignature + optional `redacted` flag
- `ImageContent` — base64 data + mimeType
- `ToolCall` — id, name, arguments (parsed object), optional thoughtSignature

Key design: the generic layer does NOT include provider-specific blocks like
`ServerToolUseBlock`, `WebSearchToolResultBlock`, `MCPToolUseBlock`, etc.
Provider-specific server tools are handled inside the provider implementation,
not leaked into the generic type system.

### What we have

Our `aj-models` message types are essentially a copy of the `anthropic-sdk`
types — 13 `ContentBlockParam` variants and 10 `ContentBlock` variants including
Anthropic-specific blocks (ServerToolUse, WebSearchToolResult,
CodeExecutionToolResult, MCPToolUse, MCPToolResult, ContainerUpload). This
leaks Anthropic internals into the "generic" layer.

### What to change

- **Strip provider-specific blocks** from the generic `ContentBlockParam` and
  `ContentBlock` enums. Keep only: Text, Image, Thinking (with
  redacted flag + signature), ToolUse, ToolResult.
- **Add `redacted` flag** to ThinkingBlock in the generic layer so redacted
  thinking can round-trip correctly.
- **Add `thinking_signature`** field to the generic ThinkingBlock for multi-turn
  thinking continuity.
- **Separate UserMessage content types from AssistantMessage content types** —
  users can send text+images, assistants produce text+thinking+tool_calls. This
  prevents nonsensical combinations.
- **Add `ToolResultMessage`** as a first-class message type (currently tool
  results are content blocks inside user messages, following raw Anthropic wire
  format). This is cleaner and mirrors how tool results actually work.
- **Add `text_signature` field** to TextContent for providers that expose
  message-level signatures (OpenAI responses).
- **Add `thought_signature` field** to ToolCall for providers that attach
  encrypted reasoning context to tool calls (OpenAI).

---

## 2. Streaming Event Protocol (`aj-models`)

### What pi-ai does

A clean event protocol with lifecycle events:

```
start           — stream begins, carries initial partial AssistantMessage
text_start      — new text block started at contentIndex
text_delta      — incremental text chunk
text_end        — text block complete, carries full text
thinking_start  — new thinking block started
thinking_delta  — incremental thinking chunk
thinking_end    — thinking block complete
toolcall_start  — new tool call started
toolcall_delta  — incremental JSON for tool arguments
toolcall_end    — tool call complete, carries parsed ToolCall
done            — stream complete with final message
error           — stream failed with error message
```

Every event carries a `partial: AssistantMessage` that is continuously updated,
so consumers always have access to the full accumulated state.

### What we have

Our `StreamingEvent` enum (12 variants) is modeled after the raw Anthropic SSE
events (`MessageStart`, `UsageUpdate`, `FinalizedMessage`, `TextStart`,
`TextUpdate`, `TextStop`, `ThinkingStart`, `ThinkingUpdate`, `ThinkingStop`,
`Error`, `ParseError`, `ProtocolError`).

Critical problems:
1. **OpenAI adapters provide NO incremental streaming** — text is accumulated
   silently and emitted in bulk at finalize. Users see no output until the
   entire response is done.
2. **No `toolcall_start`/`toolcall_delta`/`toolcall_end` events** — tool call
   streaming is not surfaced.
3. **No partial message accumulation** — events don't carry an evolving snapshot
   of the full message.

### What to change

- **Redesign `StreamingEvent`** to match the pi-ai protocol: `start`,
  `text_start`, `text_delta`, `text_end`, `thinking_start`, `thinking_delta`,
  `thinking_end`, `toolcall_start`, `toolcall_delta`, `toolcall_end`, `done`,
  `error`.
- **Every event carries a `partial` AssistantMessage** — accumulated state.
- **Both Anthropic and OpenAI adapters MUST emit incremental events** — this
  means the OpenAI adapter needs a proper stream processor that emits
  `text_delta` as chunks arrive, not batch at the end.
- **Add `contentIndex`** to block-level events so consumers know which content
  block is being updated.

---

## 3. AssistantMessage Event Stream

### What pi-ai does

`AssistantMessageEventStream` is an async-iterable push stream with a
`.result()` method that returns a `Promise<AssistantMessage>`. This lets
consumers either iterate events for real-time UI, or just `await
stream.result()` for the final message.

### What we have

We return `Pin<Box<dyn Stream<Item = StreamingEvent>>>` — a raw futures stream.
No `.result()` convenience.

### What to change

- **Create an `InferenceStream` wrapper** that:
  - Implements `Stream<Item = StreamingEvent>`
  - Has a `.result() -> Future<AssistantMessage>` method
  - Internally accumulates the partial message and resolves on `done`/`error`
- The `Model` trait's `run_inference_streaming` should return this type instead
  of a raw boxed stream.

---

## 4. Model Metadata & Registry

### What pi-ai does

Each `Model` carries rich metadata:
- `id`, `name`, `api`, `provider`, `baseUrl`
- `reasoning: bool` — whether model supports extended thinking
- `input: ("text" | "image")[]` — supported input modalities
- `cost: { input, output, cacheRead, cacheWrite }` — per-million-token pricing
- `contextWindow` — max context size
- `maxTokens` — max output tokens
- `headers` — custom HTTP headers
- `compat` — provider-specific compatibility overrides

A `calculateCost(model, usage)` function computes dollar costs from token counts.
A generated model registry provides `getModel(provider, id)`, `getModels(provider)`,
`getProviders()`.

### What we have

- Our `Model` trait only exposes `model_name()` and `model_url()`.
- max_tokens is hardcoded to 32,000 everywhere.
- No cost tracking, no context window info, no input modality info.
- No model registry — model selection is purely by string name.

### What to change

- **Add model metadata struct** with: id, name, provider, max_output_tokens,
  context_window, supports_thinking, supports_images, cost_per_million
  (input/output/cache_read/cache_write).
- **Add `model_metadata(&self) -> &ModelMetadata`** to the `Model` trait.
- **Add `calculate_cost(metadata, usage)` function**.
- **Make max_tokens configurable** — pass it through `StreamOptions` or use
  model metadata defaults. Remove hardcoded 32,000.
- **Consider a model registry** but this can be deferred. At minimum, default
  model names/limits should be defined as constants, not inline strings.

---

## 5. Usage Tracking

### What pi-ai does

```
Usage {
    input, output, cacheRead, cacheWrite, totalTokens,
    cost: { input, output, cacheRead, cacheWrite, total }
}
```

Cost is calculated per-request using model pricing data. Usage is populated from
both `message_start` (initial) and `message_delta` (final) events on Anthropic,
and from `chunk.usage` on OpenAI with proper handling of cached_tokens and
reasoning_tokens.

### What we have

- `Usage` struct exists in `anthropic-sdk` with the right fields.
- `aj-models` `StreamingEvent::UsageUpdate` carries usage.
- **Bug**: `openai_ng.rs` maps `reasoning_tokens` to `cache_read_input_tokens` —
  semantically wrong.
- No cost calculation.
- OpenAI adapter in `openai.rs` (async-openai) doesn't properly separate cached
  vs non-cached tokens.

### What to change

- **Fix the `openai_ng.rs` usage mapping bug**.
- **Add cost calculation** using model metadata.
- **Ensure both OpenAI adapters properly handle** `prompt_tokens_details.cached_tokens`
  and `completion_tokens_details.reasoning_tokens`.
- **Include usage in the `done` event's final AssistantMessage**, not as a
  separate `UsageUpdate` event.

---

## 6. Anthropic Provider

### What pi-ai does well that we don't

| Feature | pi-ai | aj |
|---|---|---|
| **Adaptive thinking** (Opus 4.6, Sonnet 4.6) | `type: "adaptive"` + `output_config.effort` | Not supported — only budget-based |
| **Effort levels** | low/medium/high/max | Only Low/Medium/High mapped to budget_tokens |
| **Thinking budget for older models** | Configurable via options + level-based defaults | Hardcoded Low=4k, Med=10k, High=32k |
| **Interleaved thinking** | Beta header when needed, skipped for 4.6 models | Commented out |
| **Redacted thinking round-trip** | Stores `data` in signature, sends back as `redacted_thinking` | Type exists but no round-trip logic |
| **Cache retention levels** | none/short/long, long uses `ttl: "1h"` | Only "ephemeral" — no retention control |
| **Signature deltas** | Accumulated during streaming for thinking blocks | Handled |
| **Tool choice** | auto/any/none/specific tool | Not exposed through generic layer |
| **Temperature** | Correctly skipped when thinking is enabled | Not checked |
| **AbortSignal** | Passed to client for request cancellation | No cancellation support |
| **Metadata (user_id)** | Forwarded for abuse tracking | Not supported |
| **Unicode sanitization** | Strips unpaired surrogates before sending | No sanitization |
| **Partial JSON parsing** | Uses `partial-json` library for streaming tool args | Reassembles full JSON from deltas, no partial parse |
| **Cross-provider message transform** | Strips thinking for different models, normalizes tool IDs | No cross-provider handling |
| **Orphaned tool call handling** | Inserts synthetic error results for incomplete tool calls | No handling — would crash |
| **Error/aborted message filtering** | Drops errored/aborted assistant messages from replay | No filtering |
| **Fine-grained tool streaming beta** | Enabled via header | Not used |
| **`pause_turn` stop reason** | Mapped to "stop" for resubmit | Listed but not tested |

### What to change

**High priority:**
1. Add adaptive thinking support (`type: "adaptive"` + effort levels) for
   Opus 4.6 and Sonnet 4.6.
2. Make thinking budget configurable with sane level-based defaults.
3. Enable interleaved thinking beta header (conditionally for pre-4.6 models).
4. Skip temperature when thinking is enabled.
5. Support request cancellation via a cancellation mechanism (tokio
   CancellationToken or similar).
6. Implement redacted thinking round-trip: store opaque `data`, send back as
   `redacted_thinking` block.

**Medium priority:**
7. Add cache retention levels (none/short/long with TTL).
8. Forward `tool_choice` option through the generic layer.
9. Add unicode surrogate sanitization.
10. Add partial JSON parsing for streaming tool call arguments.

**Lower priority:**
11. Add message transformation layer (strip thinking for cross-model, normalize
    tool IDs, handle orphaned tool calls, filter errored messages).
12. Forward metadata (user_id) for abuse tracking.
13. Enable fine-grained tool streaming beta header.

---

## 7. OpenAI Provider

### What pi-ai does well that we don't

| Feature | pi-ai | aj |
|---|---|---|
| **Real-time streaming** | Full incremental text_delta/thinking_delta/toolcall_delta | No streaming — batched at end |
| **Reasoning content** | Handles `reasoning_content`, `reasoning`, `reasoning_text` fields | Not handled |
| **reasoning_effort** | Mapped from thinking levels, provider-aware | ThinkingConfig either ignored or mapped to ReasoningEffort |
| **developer role** | Used for system prompt with reasoning models | Not used |
| **stream_options.include_usage** | Enabled for token usage in streaming | Not set |
| **Image support** | base64 data URLs in user messages + tool results | Panics on non-text content |
| **Thinking as text fallback** | For providers that don't support native thinking blocks | No fallback |
| **Tool result images** | Extracted and sent as follow-up user message with image | Not supported |
| **max_completion_tokens vs max_tokens** | Auto-detected per provider | Inconsistent between two adapters |
| **store: false** | Prevents OpenAI from storing conversations | Not set |
| **Compat layer** | Detects provider capabilities from URL/config | No compat detection |
| **Cached tokens** | Subtracted from prompt_tokens to get true input | Not handled correctly |
| **Partial JSON parsing** | `partial-json` library for streaming tool args | Not used |
| **Content filter stop reason** | Mapped to error with message | Not distinct |
| **Unicode sanitization** | Strips unpaired surrogates | Not done |

### What to change

**Critical (OpenAI is broken without these):**
1. **Implement real-time streaming** in the OpenAI adapter — emit `text_delta`,
   `thinking_delta`, `toolcall_delta` as chunks arrive. This is the #1 issue.
2. **Stop panicking on unexpected content blocks** — gracefully skip or convert
   them.

**High priority:**
3. Handle `reasoning_content`/`reasoning`/`reasoning_text` fields for thinking
   models.
4. Use `developer` role for system prompts with reasoning models.
5. Set `stream_options: { include_usage: true }` for proper token tracking.
6. Support images in user messages (base64 data URLs).
7. Use `max_completion_tokens` instead of deprecated `max_tokens`.
8. Fix the usage bug in `openai_ng.rs` (reasoning_tokens mapped to wrong field).

**Medium priority:**
9. Add `store: false` to prevent conversation storage.
10. Handle cached_tokens properly (subtract from prompt_tokens).
11. Add partial JSON parsing for streaming tool arguments.
12. Map `content_filter` finish reason to error with descriptive message.
13. Add unicode surrogate sanitization.

**Lower priority:**
14. Support tool result images (as follow-up user message).
15. Add compat layer for detecting provider capabilities.
16. Handle `reasoning_details` for encrypted reasoning on tool calls.

---

## 8. Consolidate OpenAI Implementations

### Current state

We have two OpenAI adapters:
- `openai.rs` — uses the third-party `async-openai` crate
- `openai_ng.rs` — uses our in-repo `openai-sdk` crate

Both have nearly identical stream processors with the same limitations
(no real-time streaming, panics on unexpected content). The `openai.rs`
adapter defaults to gpt-4o, `openai_ng.rs` defaults to gpt-5.

### What to change

- **Consolidate to a single OpenAI adapter** using our in-repo `openai-sdk`.
  Remove the `async-openai` dependency.
- The in-repo SDK gives us control over types and can be kept minimal.
- Fix the URL path inconsistency in `openai-sdk` (streaming path missing
  `/v1` prefix).
- Apply all streaming fixes from section 7 to the single adapter.

---

## 9. Error Handling

### What pi-ai does

- Errors during streaming are encoded IN the stream as an `error` event with
  `stopReason: "error"` and `errorMessage` on the AssistantMessage.
- Abort/cancellation produces `stopReason: "aborted"`.
- The stream always terminates cleanly (either `done` or `error`).
- No panics.

### What we have

- `anthropic-sdk` panics on malformed SSE events.
- OpenAI adapters panic on unexpected content block types.
- Inconsistent error types: `anyhow` in some paths, `ClientError` in others.
- `ModelError` only has `Client` and `Json` variants — no structured API errors.

### What to change

- **Remove all panics** from SDK and adapter code. Convert to error events or
  graceful degradation.
- **Standardize on `ClientError`** in the anthropic-sdk (replace the `anyhow`
  usage in the non-streaming path).
- **Ensure streams always terminate** with either `done` or `error` — never
  silently drop.
- **Add `aborted` stop reason** for cancellation.
- **Consider adding structured `ApiError` variants** to `ModelError` (rate
  limit, auth, overloaded) so the agent can make retry decisions.

---

## 10. Stream Options

### What pi-ai does

```
StreamOptions {
    temperature, maxTokens, signal (AbortSignal),
    apiKey, cacheRetention, sessionId,
    headers, onPayload, maxRetryDelayMs, metadata
}
```

Plus provider-specific extensions:
- `AnthropicOptions` adds: thinkingEnabled, thinkingBudgetTokens, effort,
  interleavedThinking, toolChoice, client
- `OpenAICompletionsOptions` adds: toolChoice, reasoningEffort

A `SimpleStreamOptions` layer adds `reasoning: ThinkingLevel` and
`thinkingBudgets` for a simpler API that maps to provider-specific options.

### What we have

The `Model::run_inference_streaming` takes `thinking: Option<ThinkingConfig>` as
a separate parameter. No other options are configurable — temperature, max_tokens,
tool_choice, cancellation are all either hardcoded or missing.

### What to change

- **Add an `InferenceOptions` struct**:
  ```rust
  pub struct InferenceOptions {
      pub temperature: Option<f32>,
      pub max_tokens: Option<u32>,
      pub thinking: Option<ThinkingLevel>,
      pub tool_choice: Option<ToolChoice>,
      pub cache_retention: Option<CacheRetention>,
      pub metadata: Option<HashMap<String, String>>,
  }
  ```
- **Add `ThinkingLevel` enum**: Minimal, Low, Medium, High, Max (with Max only
  for Opus 4.6).
- **Add `ToolChoice` enum**: Auto, Any, None, Tool(String).
- **Add `CacheRetention` enum**: None, Short, Long.
- Pass `InferenceOptions` to `run_inference_streaming` instead of bare `thinking`.

---

## 11. Implementation Priority

### Phase 1 — Fix what's broken
1. OpenAI real-time streaming (section 7.1)
2. Remove panics (section 9)
3. Fix openai_ng usage bug (section 7.8)
4. Consolidate to single OpenAI adapter (section 8)

### Phase 2 — Core model layer redesign
5. Redesign generic message types (section 1)
6. Redesign streaming event protocol (section 2)
7. Add InferenceStream wrapper (section 3)
8. Add InferenceOptions (section 10)

### Phase 3 — Anthropic parity
9. Adaptive thinking for 4.6 models (section 6.1)
10. Configurable thinking budgets (section 6.2)
11. Interleaved thinking (section 6.3)
12. Redacted thinking round-trip (section 6.6)
13. Cache retention levels (section 6.7)
14. Temperature/thinking guard (section 6.4)

### Phase 4 — OpenAI parity
15. Reasoning content handling (section 7.3)
16. Developer role (section 7.4)
17. Proper usage tracking with cached tokens (section 7.10)
18. Image support (section 7.6)
19. max_completion_tokens (section 7.7)

### Phase 5 — Polish
20. Model metadata + registry (section 4)
21. Cost calculation (section 5)
22. Message transformation layer (section 6.11)
23. Unicode sanitization (sections 6.9, 7.13)
24. Partial JSON parsing (sections 6.10, 7.11)
25. Cancellation support (section 6.5)
