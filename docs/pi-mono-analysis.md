# pi-mono Analysis: Lessons for aj-models

Analysis of [badlogic/pi-mono](https://github.com/badlogic/pi-mono), a TypeScript
monorepo providing a unified LLM abstraction layer (`pi-ai`), agent runtime
(`pi-agent-core`), and coding agent CLI (`pi-coding-agent`). The goal is to
identify patterns and design decisions that could improve `aj-models`.

## 1. Architecture Overview

pi-mono is a TypeScript monorepo with 7 npm-workspace packages. The dependency
chain most relevant to us is:

```
pi-ai  →  pi-agent-core  →  pi-coding-agent
```

`pi-ai` is the foundation — a unified LLM abstraction layer that supports 22
providers across 10 API protocols. It's analogous to our `aj-models` crate but
substantially more mature in its provider coverage and cross-provider
compatibility story.

## 2. Type System Comparison

### 2.1 Content Blocks

**pi-mono** uses a flat set of content types:

| Type | Fields | Notes |
|------|--------|-------|
| `TextContent` | `text`, `textSignature?` | Signature tracks OpenAI Responses API message identity |
| `ThinkingContent` | `thinking`, `thinkingSignature?`, `redacted?` | Signature is opaque provider data for multi-turn continuity |
| `ImageContent` | `data` (base64), `mimeType` | |
| `ToolCall` | `id`, `name`, `arguments`, `thoughtSignature?` | `thoughtSignature` is Google-specific |

**aj-models** has a richer block taxonomy (`ContentBlockParam` / `ContentBlock`):
TextBlock, ImageBlock, DocumentBlock, ThinkingBlock, RedactedThinkingBlock,
ToolUseBlock, ToolResultBlock, ServerToolUseBlock, WebSearchToolResultBlock,
CodeExecutionToolResultBlock, MCPToolUseBlock, MCPToolResultBlock,
ContainerUploadBlock.

**Takeaway**: Our content block model is already more detailed than pi-mono's.
pi-mono keeps it deliberately minimal — the 4 core types cover what actually
flows through the abstraction. Our extra variants (ServerToolUse, MCP, etc.) are
Anthropic-specific and leak provider details into the shared type. Consider
whether some of these should be internal to the Anthropic provider rather than
part of the shared type.

### 2.2 Messages

**pi-mono** uses 3 message roles:

```typescript
UserMessage    { role: "user",       content, timestamp }
AssistantMessage { role: "assistant", content, api, provider, model, usage, stopReason, ... }
ToolResultMessage { role: "toolResult", toolCallId, toolName, content, isError, ... }
```

Key insight: **every assistant message records which `api`, `provider`, and
`model` produced it**. This is critical for cross-provider replay — the system
uses this metadata to decide whether thinking signatures can be replayed or must
be converted to plain text.

**aj-models** stores messages as `MessageParam { role, content }` without
tracking which provider produced them.

**Recommendation**: Add provider/model metadata to assistant messages. This
enables correct cross-provider message transformation (see §5).

### 2.3 Usage and Cost

**pi-mono** embeds dollar costs directly in usage:

```typescript
interface Usage {
  input: number;
  output: number;
  cacheRead: number;
  cacheWrite: number;
  totalTokens: number;
  cost: { input, output, cacheRead, cacheWrite, total };  // dollars
}
```

Costs computed via `calculateCost(model, usage)` using per-model pricing from
the model registry (`cost: { input, output, cacheRead, cacheWrite }` in $/M
tokens).

**aj-models** tracks tokens but not costs. The `Usage` type has `input_tokens`,
`output_tokens`, plus optional cache fields. No pricing data.

**Recommendation**: Add a cost model. Each model definition should carry pricing
info, and usage should include computed dollar costs. This is important for the
agent layer to make cost-aware decisions and for users to understand spend.

## 3. Provider Registry and Lazy Loading

### 3.1 Registry

**pi-mono** has a formal `ApiProvider` registry:

```typescript
interface ApiProvider<TApi, TOptions> {
  api: TApi;
  stream: StreamFunction<TApi, TOptions>;
  streamSimple: StreamFunction<TApi, SimpleStreamOptions>;
}
```

Registration: `registerApiProvider()` / `getApiProvider()` /
`unregisterApiProviders(sourceId)`. The `sourceId` parameter enables
removing providers registered by a specific extension/plugin.

**aj-models** uses a factory function (`create_model`) with string matching:

```rust
match model_args.api.as_str() {
    "anthropic" => ...,
    "openai" => ...,
    "openai-ng" => ...,
}
```

**Recommendation**: The factory function is fine for now. If we add more
providers or want plugin-style extensibility, move to a registry. Not urgent.

### 3.2 Lazy Loading

pi-mono lazily loads provider implementations via dynamic `import()`. Only the
providers you actually use get loaded. This keeps startup fast.

Our Rust binary links all providers at compile time. Not a concern for us — Rust
binaries don't have the same startup-time sensitivity as JS.

## 4. Streaming Architecture

### 4.1 Event Protocol

**pi-mono** defines a strict event protocol:

```
start → (text_start → text_delta* → text_end |
         thinking_start → thinking_delta* → thinking_end |
         toolcall_start → toolcall_delta* → toolcall_end)* →
        (done | error)
```

Each event carries a `contentIndex` (position in the growing content array) and
a `partial` reference to the in-progress message. Terminal events carry the
final `AssistantMessage`.

The `AssistantMessageEventStream` class extends a generic `EventStream<T, R>`
implementing `AsyncIterable<T>` with a `result(): Promise<R>` for getting the
final message without iterating.

**aj-models** has a similar but less strict protocol:

```
MessageStart → (TextStart → TextUpdate* → TextStop |
                ThinkingStart → ThinkingUpdate* → ThinkingStop)* →
               UsageUpdate → FinalizedMessage | Error
```

We provide both incremental diffs and full snapshots in update events, which is
nice. But we don't have explicit tool call streaming events — tool calls only
appear in the finalized message.

**Recommendation**: Add tool call streaming events (`ToolCallStart`,
`ToolCallDelta`, `ToolCallStop`). This enables:
- Displaying tool call names as soon as they're known (before arguments finish)
- Streaming tool call arguments to the UI for large arguments
- Earlier tool execution decisions in the agent layer

### 4.2 Error Handling Contract

**pi-mono** has a strict contract: stream functions must **never throw**. All
errors are encoded as events in the stream (`error` event with
`stopReason: "error"` or `"aborted"`).

```typescript
// Contract: Must NOT throw. Errors encoded in the stream.
type StreamFunction = (model, context, options?) => AssistantMessageEventStream;
```

Every provider wraps its async logic in try/catch and pushes error events on
failure.

**aj-models** returns `Result<Stream, ModelError>` from
`run_inference_streaming`. This means the caller has to handle two error paths:
the Result error and stream-level errors. pi-mono's approach of encoding
everything in the stream simplifies the consumer.

**Recommendation**: Consider making our stream function infallible (always return
a stream, encode errors as events). This simplifies the agent loop — it only
needs to handle one error path.

## 5. Cross-Provider Message Transformation

This is one of pi-mono's most sophisticated features and something we lack
entirely.

### 5.1 The Problem

When replaying a conversation with a different provider (or even a different
model from the same provider), certain message fields are provider-specific and
will cause errors or incorrect behavior:

- **Thinking signatures**: Opaque encrypted data tied to the specific model.
  Anthropic thinking signatures can't be sent to OpenAI and vice versa.
- **Text signatures**: OpenAI Responses API message identity data.
- **Tool call IDs**: Different providers have different ID format constraints
  (Anthropic: `^[a-zA-Z0-9_-]+$` max 64; OpenAI: different format).
- **Errored/aborted messages**: May have partial/incomplete content that causes
  API errors on replay.
- **Orphaned tool calls**: If an assistant message has tool calls but no matching
  tool results (e.g., due to interruption), the API will reject the request.

### 5.2 pi-mono's Solution

`transformMessages(messages, model, normalizeToolCallId?)` performs these
transformations before every API call:

1. **Thinking blocks**: If message was from a different model, thinking blocks
   with signatures are converted to plain text. Redacted thinking blocks are
   dropped for cross-model replay. Empty thinking blocks removed.
2. **Text signatures**: Stripped for cross-model replay.
3. **Tool call IDs**: Normalized via provider-specific callback.
4. **Errored/aborted messages**: Completely skipped.
5. **Orphaned tool calls**: Synthetic error tool results inserted.

This is what enables pi-mono to seamlessly switch between providers
mid-conversation.

### 5.3 Recommendation

Implement message transformation. This is a high-value feature because:

- It enables provider switching mid-conversation (resilience, cost optimization).
- It prevents subtle API errors from stale provider-specific data.
- It handles edge cases (interrupted tool calls, partial messages) that would
  otherwise cause hard-to-debug failures.

Prerequisites:
- Store provider/model metadata on assistant messages (§2.2).
- Define per-provider tool call ID normalization.

## 6. Thinking / Reasoning Abstraction

### 6.1 Unified Thinking Levels

**pi-mono** introduces a `ThinkingLevel` enum:
`"minimal" | "low" | "medium" | "high" | "xhigh"`

The `streamSimple` / `completeSimple` functions accept a `ThinkingLevel` and map
it to provider-specific parameters:

| Provider | Mechanism |
|----------|-----------|
| Anthropic (Opus 4.6/Sonnet 4.6) | Adaptive thinking with effort levels |
| Anthropic (older) | Budget tokens: 1024/2048/8192/16384 |
| OpenAI | `reasoning_effort` parameter |
| OpenRouter | `reasoning: { effort }` |
| z.ai / Qwen | `enable_thinking: boolean` |

**aj-models** has `ThinkingConfig` with 3 levels mapping to budget tokens:
Low (4k), Medium (10k), High (31,999).

**Recommendation**: Our approach is reasonable but misses:
- **Adaptive thinking**: Newer Anthropic models (Opus 4.6, Sonnet 4.6) support
  effort-level thinking (`adaptive` type) rather than budget-based. We should
  support this.
- **`xhigh` / `max` effort**: For Opus 4.6 and GPT-5.2+.
- **Provider-specific mapping**: OpenAI's `reasoning_effort` is different from
  Anthropic's budget tokens. The mapping should happen in the provider, not the
  shared type.

## 7. Model Registry and Pricing

**pi-mono** has a comprehensive model registry (`models.generated.ts`, ~14,000
lines) with every model carrying:

```typescript
interface Model<TApi> {
  id: string;
  name: string;
  api: TApi;
  provider: Provider;
  baseUrl: string;
  reasoning: boolean;
  input: ("text" | "image")[];
  cost: { input, output, cacheRead, cacheWrite };  // $/M tokens
  contextWindow: number;
  maxTokens: number;
  headers?: Record<string, string>;
  compat?: OpenAICompletionsCompat;
}
```

**aj-models** constructs models in the factory function with hardcoded defaults.
There's no model registry, no pricing data, no capability metadata.

**Recommendation**: Build a model registry. Each model should carry:
- Pricing info (for cost tracking)
- Capability flags (reasoning, image input, etc.)
- Context window and max output tokens
- Provider-specific compatibility settings

This can start small — just our supported models — and grow.

## 8. Prompt Caching

**pi-mono** has explicit cache control:

```typescript
cacheRetention?: "none" | "short" | "long";
```

Provider-specific mapping:
- Anthropic: `cache_control: { type: "ephemeral" }` for short,
  `{ type: "ephemeral", ttl: "1h" }` for long
- OpenAI Responses: `prompt_cache_key` (session-based), `prompt_cache_retention: "24h"` for long
- Applied to system prompt blocks and last user message

**aj-models** already applies ephemeral cache control to system prompts and the
last user/assistant messages in the Anthropic provider. But it's hardcoded, not
configurable.

**Recommendation**: Make cache retention configurable (at minimum a "none" /
"short" / "long" toggle). This matters for cost — cache reads are significantly
cheaper than fresh input tokens.

## 9. OpenAI Compatibility Layer

pi-mono handles enormous variation among OpenAI-compatible providers via an
`OpenAICompletionsCompat` interface:

```typescript
interface OpenAICompletionsCompat {
  supportsStore?: boolean;
  supportsDeveloperRole?: boolean;
  supportsReasoningEffort?: boolean;
  maxTokensField?: "max_completion_tokens" | "max_tokens";
  requiresToolResultName?: boolean;
  requiresAssistantAfterToolResult?: boolean;
  requiresThinkingAsText?: boolean;
  thinkingFormat?: "openai" | "openrouter" | "zai" | "qwen";
  supportsStrictMode?: boolean;
  // ... more
}
```

Auto-detected from provider name and baseUrl in `detectCompat()`.

**aj-models** has two OpenAI implementations (`openai.rs` using async-openai,
`openai_ng.rs` using our custom SDK) but no compatibility abstraction for
OpenAI-compatible providers.

**Recommendation**: If we want to support providers like Groq, Together, etc.
through our OpenAI provider, we'll need a compat layer. Not urgent unless we're
adding more providers soon.

## 10. Context Overflow Detection

**pi-mono** has dedicated overflow detection (`isContextOverflow`) with pattern
matching against 15+ provider-specific error messages. Two detection modes:
- **Error-based**: Regex matching on error messages from various providers.
- **Silent overflow**: Some providers accept overflow silently — detected by
  comparing `usage.input > contextWindow`.

**aj-models** doesn't have dedicated overflow detection.

**Recommendation**: Implement context overflow detection. The agent layer needs
to know when it hits the context limit so it can truncate, summarize, or
otherwise recover. This is essential for long-running agent sessions.

## 11. Summary of Recommendations

Ordered by impact and implementation effort:

### High Impact, Moderate Effort
1. **Tool call streaming events** — Add ToolCallStart/Delta/Stop to streaming
   protocol. Enables better UX and earlier agent decisions.
2. **Cross-provider message transformation** — Handle thinking signatures, tool
   call ID normalization, orphaned tool calls. Prerequisite: store provider/model
   on assistant messages.
3. **Context overflow detection** — Pattern-match provider error messages to
   detect overflow. Essential for long agent sessions.

### High Impact, Low Effort
4. **Adaptive thinking support** — Opus 4.6 and Sonnet 4.6 use effort-level
   thinking, not budget-based. Update our Anthropic provider.
5. **Store provider/model on assistant messages** — Small type change, big
   enabler for cross-provider features.

### Medium Impact, Moderate Effort
6. **Model registry with pricing** — Track model capabilities, pricing, context
   windows. Enables cost tracking and model selection.
7. **Configurable cache retention** — Make prompt caching controllable rather
   than hardcoded.
8. **Infallible stream function** — Encode all errors as stream events rather
   than returning Result. Simplifies the consumer.

### Lower Priority
9. **Cost tracking in Usage** — Compute dollar costs from token usage and model
   pricing. Nice for users, not blocking.
10. **OpenAI compat layer** — Only needed if we add more OpenAI-compatible
    providers.
11. **Slim down shared content block types** — Move Anthropic-specific blocks
    (MCP, ServerToolUse, etc.) out of the shared type.
