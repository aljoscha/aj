# Audit findings — openai-sdk

- **Step:** S2
- **Date:** 2026-06-02
- **Audited commit:** 5d43f02
- **Scope:** `src/openai-sdk/src/lib.rs`, `client.rs`, `types.rs`,
  `types/chat_completions.rs`, `types/common.rs`, `types/responses.rs`,
  `Cargo.toml` (incl. the in-module `#[cfg(test)]` suite in
  `chat_completions.rs`; no `tests/` dir).

## Summary

The crate is a faithful, well-organized OpenAI wire layer covering three
endpoints (Chat Completions, Responses, Codex Responses). The type
modules are cohesive and the forward-compat catch-alls (`Other(Value)`,
`FinishReason::Other`, reasoning-field aliases) are a real strength. The
two recurring themes flagged in `anthropic-sdk` hold here too, and more
sharply: the four streaming entry points present a clean `thiserror`
`ClientError`, but the **two non-streaming entry points
(`chat_completions`, `responses`) return `anyhow::Error`** — the same
split as `anthropic-sdk`, here duplicated across two methods, and both
methods are dead relative to the only consumer. Dead public surface is
also wider than `anthropic-sdk`: an unused `async-stream` dependency, two
unused non-streaming methods, `base_url()`, `Response::output_text()`,
and a raft of convenience constructors that `aj-models` never calls
(it builds the wire structs directly). No correctness or secret-handling
risks.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 4 | 2 |

## Findings

### [Major][Errors] Non-streaming `chat_completions`/`responses` return `anyhow` while the streaming paths use `ClientError` — `src/openai-sdk/src/client.rs:78,195`
**What:** `chat_completions()` (line 78) and `responses()` (line 195)
return `Result<_, anyhow::Error>` and build errors with `anyhow!`
(`client.rs:109,110,113,226,227,230`), discarding the structured
`ApiError`/`http_status`/`retry_after` classification. The streaming
methods (`chat_completions_stream`, `responses_stream`,
`codex_responses_stream`) all return the crate's `thiserror`
`ClientError` and do the full status + `Retry-After` extraction
(`client.rs:166-192,315-341`). This is the same inconsistency the
`anthropic-sdk` audit flagged, but doubled: two methods, with near-
identical error-mapping logic copy-pasted between the streaming pair
(`client.rs:173-192` vs. `322-341`).
**Why it matters:** A library boundary should present one coherent error
contract; per `CLAUDE.md`, library crates use `thiserror` and `anyhow`
is reserved for the `aj` binary. The `anyhow` returns are the sole reason
`anyhow` is a dependency of this crate. Both methods being dead (next
finding) means the inconsistency ships purely as latent surface.
**Suggested action:** Either remove `chat_completions()`/`responses()`
(see next finding) — which lets the crate drop `anyhow` entirely — or
convert them to return `ClientError` and factor the shared status →
`ApiError`/`retry_after` mapping into one helper used by all five paths
(the streaming pair already duplicate it and should share too).
**Effort:** S

### [Minor][Simplicity] Non-streaming methods and several convenience helpers are dead relative to the consumer — `src/openai-sdk/src/client.rs:74,78,195`, `src/openai-sdk/src/types/responses.rs:165,381,1084,1095,1128,1140`, `src/openai-sdk/src/types/chat_completions.rs:706,766,770`
**What:** No caller in `aj-models`/`aj` uses: `Client::chat_completions`
(non-stream), `Client::responses` (non-stream), `Client::base_url()`
(`client.rs:74`), `Response::output_text()` (`responses.rs:381`), the
`ResponseInstructions: From<String>/<&str>` impls (`responses.rs:165`),
`CreateResponseRequest::new`/`CreateChatCompletionRequest::new`
(`responses.rs:1084`, `chat_completions.rs:706`),
`ResponseInputItem::user_text`/`function_call_output`
(`responses.rs:1095,1128`), `ResponseTool::function` (`responses.rs:1140`),
and `ChatCompletionUserContent::text`/`with_image`
(`chat_completions.rs:766,770`). `aj-models` constructs these wire
structs directly via field literals and uses only `developer`/`system`,
`system_text`/`developer_text`, `assistant`/`tool` variants. The
streaming endpoints are the only `Client` methods actually called.
**Why it matters:** Same theme as `anthropic-sdk` but broader: a "thin
client" has accreted a convenience layer past what its single consumer
needs, widening the surface that must stay correct and tested, and the
two dead non-streaming methods carry the divergent `anyhow` error path.
**Suggested action:** Decide the surface policy with the user (internal
wire layer vs. general SDK). If internal: prune the unused methods and
constructors, dropping `anyhow`. If general: keep them but state that
intent in the module docs so the audit can distinguish "public on
purpose" from "leaked", and unify their error type per the Major finding.
**Effort:** M

### [Minor][Dependencies] `async-stream` is declared but never used — `src/openai-sdk/Cargo.toml:8`
**What:** `async-stream` is in `[dependencies]` (line 8) but there is no
`async_stream::` reference anywhere in the crate (`rg async_stream`
returns nothing). The streaming methods use
`response.bytes_stream().eventsource().filter_map(...)`, not
`async_stream`.
**Why it matters:** Unused dependency — extra compile time and a
misleading signal about how streaming is implemented. Dependency-hygiene
miss the workspace lints don't catch.
**Suggested action:** Remove `async-stream` from `Cargo.toml`.
**Effort:** S

### [Minor][Errors] Streaming error-mapping logic is duplicated verbatim — `src/openai-sdk/src/client.rs:166-192,315-341`
**What:** The non-2xx handling in `chat_completions_stream`
(`client.rs:166-192`) and `responses_stream_at_path`
(`client.rs:315-341`) is byte-for-byte identical: extract `http_status`,
read `retry-after`, parse `ApiErrorResponse` (falling back to a synthetic
`ApiError`), and build `ClientError::ApiError` / `InternalError`. The SSE
`filter_map` bodies (`client.rs:141-162` vs. `292-311`) are likewise
near-identical apart from the event type they deserialize.
**Why it matters:** Two copies of the error/`Retry-After` contract drift
independently; a fix to one (e.g. handling a new header) can miss the
other. `responses_stream_at_path` already demonstrates the right pattern
by sharing path logic between `responses_stream` and
`codex_responses_stream`.
**Suggested action:** Extract a `map_error_response(status, headers,
body) -> ClientError` helper and a generic
`parse_sse_stream::<T: DeserializeOwned>()` so all streaming paths share
one error-mapping and one SSE-parsing implementation.
**Effort:** M

### [Minor][Contracts] SSE deserialization treats response and chat-completions endpoints differently, and the `[DONE]` sentinel for Responses is undocumented — `src/openai-sdk/src/client.rs:144,295`
**What:** Both streaming parsers special-case `event.data == "[DONE]"` to
end the stream (`client.rs:144,295`). For Chat Completions that sentinel
is part of the wire protocol; for the Responses/Codex endpoints OpenAI
signals completion via the `response.completed` event, and `[DONE]` is
not part of that protocol — the check is harmless but unexplained, and a
reader cannot tell whether it's load-bearing. Neither parser documents
that a successful 2xx stream can still carry an `error` event
(`ResponseStreamEvent::Error`, `responses.rs:1067`) that the SDK passes
through as `Ok(event)` rather than as a `ClientError`.
**Why it matters:** The boundary's behavior on mid-stream errors and on
the `[DONE]` sentinel is enforced by convention, not types or docs;
`aj-models` must know that a transport-level `Err` and a protocol-level
`error` event are surfaced through different channels.
**Suggested action:** Add a short note on the streaming methods (or the
shared helper) describing the termination conditions: transport `Err` →
`ClientError::InternalError`; `[DONE]` → clean end; `error` event →
yielded as `Ok(ResponseStreamEvent::Error)` for the consumer to classify.
Drop the `[DONE]` check from the Responses path if it's not part of that
protocol, or note why it's kept defensively.
**Effort:** S

### [Nit][Comments] `ApiError` mixes `thiserror::Error` derive with a hand-written `Display` — `src/openai-sdk/src/types/common.rs:9,20`
**What:** `ApiError` derives `thiserror::Error` (line 9) but provides a
manual `impl Display` (line 20) instead of an `#[error("…")]` attribute,
so the derive contributes only `std::error::Error`. This is the exact
pattern flagged in `anthropic-sdk` (`messages.rs` `ApiError`) — confirmed
to recur here.
**Why it matters:** Mild inconsistency; a reader must check whether the
derive or the manual impl supplies the message. `ClientError`
(`client.rs:345`) uses `#[error("…")]`, so the crate is internally
inconsistent.
**Suggested action:** Replace the manual `Display` with
`#[error("OpenAI API error: {message}")]` on the struct, or add a
one-line note if the manual impl is deliberate.
**Effort:** S

### [Nit][Style] Section-divider banner comments are inconsistent across the two type modules — `src/openai-sdk/src/types/responses.rs:10`, `src/openai-sdk/src/types/chat_completions.rs:8`
**What:** `responses.rs` uses full-width `// ---...` banner separators
between sections; `chat_completions.rs` uses bare `// Request types`
style comments. Both are fine individually but the two files that make up
the same `types` boundary read differently.
**Why it matters:** Cosmetic only; minor friction when navigating
between the two sibling modules.
**Suggested action:** Pick one section-comment style and apply it to both
files (low priority).
**Effort:** S

## What's good

- **Forward-compat catch-alls.** `ResponseOutputItem::Other(Value)`,
  `ResponseStreamEvent::Other`, `ResponseAnnotation::Other`,
  `ResponseTool::Other`, `ResponseIncludable::Other`, and
  `FinishReason::Other(String)` all preserve unrecognized wire shapes so
  schema evolution doesn't break the parser. The intent is stated in the
  `lib.rs` module doc and at each enum. This is the right design for a
  client that trails a fast-moving API.
- **`FinishReason` custom (de)serialize.** Hand-rolled `Serialize`/
  `Deserialize` around `from_wire`/`as_wire` with an `Other` fallback and
  an `"end"`→`Stop` alias; documented and round-trip tested for known
  values, the alias, and the unknown fallback. Good boundary testing.
- **Reasoning-field compatibility.** `reasoning_content` with
  `alias = "reasoning"`/`"reasoning_text"` on both the response message
  and the stream delta, with a clear comment on why OpenAI-compatible
  providers differ and that native OpenAI leaves it `None`. The
  `cache_write_tokens` field carries an excellent contract note on how
  callers should reconcile it with `cached_tokens`.
- **Codex/Responses path sharing.** `responses_stream` and
  `codex_responses_stream` delegate to `responses_stream_at_path`, with a
  doc comment stating the two endpoints differ only in URL path. This is
  the right way to avoid duplication — and the model the error-mapping
  duplication (Minor finding above) should follow.
- **`ClientError` design.** Structured `thiserror` enum with
  `http_status()`/`retry_after()` accessors, matching `anthropic-sdk`'s
  shape — a good cross-SDK convention to standardize on.
- **`debug_log_request` gating.** Gated on `tracing::enabled!` so the
  body serialization cost is skipped when DEBUG is off, with the rationale
  in the doc comment. Same sound pattern as `anthropic-sdk`.
- **Deprecated wire fields are annotated.** `#[deprecated]` on
  `max_tokens`, `functions`, `function_call`, `seed`, `user`, etc., keeps
  the wire model honest about what OpenAI has superseded.

## Boundary & architecture notes

Dependency direction is correct: the crate depends only on
`reqwest`/`futures`/`eventsource-stream`/`serde`/`serde_json`/`thiserror`/
`tracing` (plus the unused `async-stream`) and is consumed by `aj-models`
(`openai/provider.rs`, `openai/responses.rs`, `openai/codex.rs`). No
reverse edges, no `aj_*` imports — it sits as a leaf under `aj-models`
exactly as the `CLAUDE.md` graph intends.

Module boundaries are clean. `types/common.rs` is a genuine shared seam,
not a dumping ground: it holds exactly the enums/structs used by *both*
the chat-completions and responses surfaces (`ReasoningEffort`,
`Verbosity`, `PromptCacheRetention`, `ServiceTier`, `JsonSchemaDefinition`,
and the `ApiError`/`ApiErrorResponse` shared by the HTTP client). Each
endpoint keeps its own `Usage`/`FinishReason`/message types locally, which
is correct because the two surfaces genuinely differ. No type leaks across
the two endpoint modules.

Public-surface concern is the same as `anthropic-sdk`, larger in degree:
the wire types are necessarily broadly `pub`, but the *client* and the
*convenience constructor* layers have drifted well past the single
consumer (two dead non-streaming methods, `base_url`, `output_text`, a
dozen unused constructors). For X1 synthesis: this is the second SDK
exhibiting the "internal wire layer vs. general SDK" ambiguity — the
surface policy should be decided once and applied to both crates.

`anyhow` is a dependency only because of the two non-streaming methods;
resolving the Major finding lets the crate drop it (mirroring the
`anthropic-sdk` conclusion). `async-stream` can be dropped outright.

## Test assessment

Tests are in-module under `#[cfg(test)]` per convention and are
well-targeted at the trickiest serde boundary: `FinishReason` round-trips
(known values, `"end"` alias, unknown→`Other`), the `reasoning_content`
aliases on both the stream delta and the response message, and
`cache_write_tokens` presence/skip. Fixtures are inline JSON literals,
readable and not brittle. No wall-clock/network/filesystem coupling.

Gaps worth noting:
- **No HTTP-boundary tests** for any `Client` method: success body parse,
  non-2xx → `ApiError` classification, `Retry-After` capture, the
  `is_client_error`/`is_server_error` vs. "unexpected status" split, or
  the `[DONE]`/transport-error stream termination. This is the exact gap
  called out for `anthropic-sdk`, confirmed here. The error-mapping
  contract `aj-models` depends on is only validated indirectly in
  `aj-models`' provider tests (which construct `ClientError` by hand,
  e.g. `openai/codex.rs:1093`). A mock-transport (`wiremock`-style) test
  would lock down the duplicated error-mapping logic that this report
  flags as drift-prone.
- **No serde round-trip tests for the large `responses.rs` type set**
  (output items, stream events, the `Other` catch-alls). As with
  `anthropic-sdk`, the real round-trip coverage lives downstream in
  `aj-models/tests/roundtrip/openai_*.rs`; acceptable placement, but the
  SDK again relies on a downstream crate to assert its own wire
  correctness — note for synthesis.
- The `Other(Value)` catch-alls and `FinishReason::NetworkError` (a
  vendor extension) are untested for the catch/fallthrough behavior they
  exist to guarantee.

## Cross-cutting themes to bubble up

- **Error-type consistency across provider SDKs (CONFIRMED, worse).**
  Both SDKs split error types between streaming (`thiserror ClientError`)
  and non-streaming (`anyhow`) paths. `openai-sdk` duplicates the
  `anyhow` path across two methods and duplicates the `ClientError`
  mapping across two streaming methods. Standardize on `ClientError`
  everywhere and let both crates drop `anyhow`.
- **Dead/unused public surface on "thin" clients (CONFIRMED, broader).**
  `openai-sdk` exposes more orphaned surface than `anthropic-sdk`: two
  dead methods, `base_url`, `output_text`, ~10 unused constructors, plus
  an unused `async-stream` dependency. Synthesis should set one surface
  policy for both SDKs.
- **`thiserror` vs. hand-written `Display` (CONFIRMED).** `ApiError` here
  repeats the `anthropic-sdk` pattern of a `thiserror` derive plus a
  manual `Display`. Pick one convention across both SDKs (and check
  `aj-models` error enums in M1).
- **HTTP-boundary test coverage (CONFIRMED).** Neither SDK tests its own
  request/error mapping; both rely on `aj-models` round-trip and
  hand-constructed-error tests. Decide consistently where wire-correctness
  and error-classification are asserted.
- **New theme: duplicated error/SSE mapping within a crate.** The two
  streaming methods copy their non-2xx handling and SSE parsing verbatim.
  Worth checking whether `aj-models`' provider layer has the same
  per-endpoint duplication (M3/M4).
