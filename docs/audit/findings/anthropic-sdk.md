# Audit findings — anthropic-sdk

- **Step:** S1
- **Date:** 2026-06-02
- **Audited commit:** adfcaca
- **Scope:** `src/anthropic-sdk/src/lib.rs`, `client.rs`, `messages.rs`,
  `stealth.rs`, `Cargo.toml` (incl. in-module `#[cfg(test)]` suites; no
  `tests/` dir; one `examples/` binary noted where relevant).

## Summary

The crate is a clean, well-documented wire layer: the message-type module
is thorough and the stealth-mode helpers are cohesive, private, and
well-tested. The crate stays mostly within its "thin async client"
charter. The main blemishes are an error-handling inconsistency between
the two request entry points and a meaningful amount of dead public
surface — a non-streaming `messages()` method that no consumer uses, a
never-constructed `ClientError` variant, and several convenience
methods/conversions that `aj-models` does not call. None of these are
correctness risks, but they widen the public boundary beyond what the
single consumer needs.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 4 | 3 |

## Findings

### [Major][Errors] Two request entry points disagree on the error type — `src/anthropic-sdk/src/client.rs:206,249`
**What:** `messages()` returns `Result<Message, anyhow::Error>` and builds
errors with `anyhow!`, while `messages_stream()` returns the crate's own
`Result<_, ClientError>`. The crate defines a `thiserror` `ClientError`
with structured `ApiError`, `http_status`, and `retry_after`, but the
non-streaming path discards all of that structure into an opaque
`anyhow::Error`. Per `CLAUDE.md`/the rubric, library crates define error
types with `thiserror`; `anyhow` is reserved for the top-level `aj`
binary.
**Why it matters:** A library boundary should present one coherent error
contract. The `anyhow` return forces any caller of `messages()` to lose
the status/retry-after classification that `messages_stream` callers get
for free, and it violates the workspace error-handling convention. It
also signals that the non-streaming path was never wired into the real
consumer (see next finding).
**Suggested action:** Make `messages()` return `Result<Message,
ClientError>` and reuse the same status/`Retry-After` extraction as the
streaming path (or remove the method per the dead-code finding below). If
kept, factor the shared error-mapping into one helper so both paths
classify identically.
**Effort:** S

### [Minor][Simplicity] Non-streaming `messages()` is dead relative to its consumer — `src/anthropic-sdk/src/client.rs:206`
**What:** `Client::messages()` (non-streaming) has no callers in
`aj-models` or `aj`; the Anthropic provider only ever calls
`messages_stream()` (`src/aj-models/src/anthropic/provider.rs:168`). Its
sole use is the `examples/thinking_signature_experiment.rs` probe.
**Why it matters:** A thin client should expose what the wire layer above
it needs. An unused second entry point with its own (divergent) error
handling is extra surface to keep correct and is the thing dragging in
the `anyhow` dependency and inconsistency above.
**Suggested action:** Decide with the user: either drop `messages()` (and
keep the experiment on `messages_stream`), or keep it but unify its error
type per the Major finding. If the non-streaming endpoint is intended to
stay supported for parity, document that intent.
**Effort:** S

### [Minor][Errors] `ClientError::ParseError` is never constructed — `src/anthropic-sdk/src/client.rs:343`
**What:** The `ParseError(String)` variant exists and is matched on by
`aj-models` (`anthropic/provider.rs:290`), but nothing in this crate ever
produces it. SSE-frame parse failures are logged and dropped
(`client.rs:277`); response-body deser failures in `messages()` propagate
as `anyhow` via `?` (`client.rs:225`), not as `ClientError::ParseError`.
**Why it matters:** A variant that can never occur is a misleading
contract: the consumer writes a `ParseError` arm that is unreachable
dead code, and a reader assumes parse failures surface here when they do
not.
**Suggested action:** Either remove the variant, or actually route the
`serde_json` failures in `messages()`/`messages_stream` through it
(preferred if `messages()` is unified onto `ClientError`).
**Effort:** S

### [Minor][Boundaries] Convenience methods and conversions unused by the wire layer above — `src/anthropic-sdk/src/client.rs:88,94,109,77`, `src/anthropic-sdk/src/messages.rs:1238,1363,1575`
**What:** Several `pub` items have no caller in `aj-models`/`aj`:
`with_betas`, `set_betas`, `set_interleaved_thinking`, `is_oauth`
(client); `Message::into_message_param`,
`ContentBlock::into_content_block_param`, and `Usage::apply_delta`
(messages). `aj-models` reimplements delta-merging against its own
unified `Usage` (`anthropic/provider.rs:1232`), so the SDK's
`apply_delta` is genuinely orphaned rather than a shared seam.
**Why it matters:** Public surface that no consumer uses widens the
boundary the crate must keep stable and tested, and the orphaned
`apply_delta` invites the impression of a shared abstraction that isn't.
This is a "thin client" that has accreted convenience helpers beyond its
single consumer's needs.
**Suggested action:** Prune the unused setters/conversions, or, if they
are deliberately part of a general-purpose SDK surface, note that intent
in the module docs so the audit (and future readers) can distinguish
"public on purpose" from "leaked". Reconcile `apply_delta` with the
consumer's `apply_usage_delta` — keep one.
**Effort:** M

### [Minor][Contracts] `messages_stream` swallows transport errors mid-stream silently — `src/anthropic-sdk/src/client.rs:290`
**What:** Once the stream is established, an `Err` from the event source
(`eventsource_stream`) is logged at `error` and mapped to `None`, i.e.
the stream simply ends with no signal to the consumer that it terminated
abnormally vs. completing. The doc comment only describes the *parse*
case ("unparseable frames are logged and dropped"), not the transport-
error case.
**Why it matters:** A consumer cannot distinguish a clean end-of-stream
from a dropped connection, which matters for retry/resume decisions in
`aj-models`. The behavior is reasonable but undocumented at the boundary.
**Suggested action:** Document that mid-stream transport errors terminate
the stream silently, or surface them (e.g. yield a terminal error event)
if the consumer needs to react.
**Effort:** S

### [Nit][Style] Redundant `use reqwest;` import — `src/anthropic-sdk/src/client.rs:6`
**What:** `use reqwest;` on line 6 is redundant with `use reqwest::Client
as ReqwestClient;` on line 7 and the later `reqwest::RequestBuilder` /
`reqwest::Error` path references; the bare crate import adds nothing.
**Why it matters:** Minor noise; rustfmt won't flag it but it's dead.
**Suggested action:** Drop line 6 and keep path-qualified `reqwest::…`
references (or import `RequestBuilder` explicitly).
**Effort:** S

### [Nit][Comments] `Display` for `ApiError` duplicates the `thiserror` derive intent — `src/anthropic-sdk/src/messages.rs:1718,1741`
**What:** `ApiError` derives `thiserror::Error` (line 1718) but provides a
hand-written `impl Display` (line 1741) rather than `#[error("…")]`
attributes on each variant. With `thiserror`, a manual `Display` means
the derive contributes only `std::error::Error`; the two mechanisms doing
adjacent jobs is easy to misread.
**Why it matters:** Mild inconsistency with the rest of the crate
(`ClientError` uses `#[error(...)]`); a reader must check whether the
derive or the manual impl wins.
**Suggested action:** Either move the messages onto `#[error("…")]`
attributes and drop the manual `impl`, or add a one-line note explaining
why the manual `Display` is preferred (e.g. shared message formatting).
**Effort:** S

### [Nit][Comments] Stealth `CLAUDE_CODE_VERSION` rationale is thin — `src/anthropic-sdk/src/stealth.rs:36`
**What:** The constant is documented as "a recent value the Anthropic
server accepts," but there's no note on how to know when it goes stale or
what symptom a rejected version produces.
**Why it matters:** This is a maintenance-sensitive magic string; the
"nota bene" is exactly the kind of non-obvious decision worth recording
fully.
**Suggested action:** Add a sentence on the failure mode (what the API
returns if the version is too old) so a future maintainer knows when/why
to bump it.
**Effort:** S

## What's good

- **Stealth module boundaries (`stealth.rs`).** Cohesive single
  responsibility (OAuth request/response name-mapping + identity
  preamble), all helpers `pub(crate)`, no leakage of these internals into
  the public API, and a thorough unit suite covering forward map, reverse
  map, pass-through, dedup, and the no-system-prompt edge case. The
  module doc clearly states the contract and the maintenance trigger.
- **Beta-header composition (`client.rs:122`).** `effective_beta_headers`
  is a small pure function with documented ordering and dedup semantics,
  and it's tested for all four regimes (API-key default, OAuth-required,
  interleaved opt-in, caller dedup). Good example of testing at the
  boundary without reaching into internals.
- **`ClientError` design (`client.rs:329`).** Structured `thiserror` enum
  with `http_status()`/`retry_after()` accessors and a doc note pointing
  callers at `aj-models`' `parse_retry_after`; `retry_after` captured
  before the body is consumed, with a comment explaining why.
- **Forward-compatible SSE handling.** Unparseable frames are skipped
  with a documented rationale (API reserves the right to add event
  types) rather than crashing the stream.
- **Type module documentation.** `messages.rs` consistently documents the
  non-obvious wire shapes (untagged unions, `caller`, compaction blocks,
  cumulative `message_delta` usage) and notes where fields are kept as
  `Value` for forward-compat. Contracts are well stated.
- **`debug_log_request` is gated** on `tracing::enabled!` so the
  serialization cost is skipped when DEBUG is off — a sensible
  hot-path-adjacent guard.

## Boundary & architecture notes

Dependency direction is correct: the crate depends only on
`reqwest`/`futures`/`serde`/`thiserror`/`tracing` and is consumed by
`aj-models` (`anthropic/provider.rs`), matching the intended graph
(`anthropic-sdk` is a leaf under `aj-models`). No reverse edges, no
`aj_*` imports.

Public-surface concerns are localized: the message types are necessarily
broadly `pub` (they're the wire contract), but the *client* surface has
drifted past its single consumer (`messages()`, several setters,
`apply_delta`, the conversions). For the X1 synthesis: confirm whether
the SDK is intended as a general-purpose published crate (justifying the
extra surface) or strictly an internal wire layer for `aj-models` (in
which case the surface should track the consumer). The same question
applies to `openai-sdk`.

`anyhow` appears as a real dependency only because of the non-streaming
`messages()` path; resolving the Major finding likely lets the crate drop
`anyhow` entirely, tightening dependency hygiene.

## Test assessment

Tests are in-module under `#[cfg(test)]` per convention and exercise the
right boundaries: header composition (all four regimes + dedup) and the
full stealth request/response mapping including edge cases (empty system
prompt, unknown/server tool names passing through). Fixtures
(`custom_tool`, the hand-built `Message`/event values) are readable and
not brittle.

Gaps worth noting:
- No test exercises `messages()` / `messages_stream` HTTP behavior
  (success body parse, error-body classification into `ApiError`,
  `Retry-After` capture, non-2xx fallthrough). A mock-transport or
  `wiremock`-style test would cover the error-mapping contract that
  `aj-models` depends on — currently only validated indirectly in
  `aj-models`' provider tests. This is the highest-value coverage gap.
- `Usage::apply_delta` (cumulative-replace semantics) is documented but
  untested; since it's orphaned (consumer reimplements it), this
  reinforces removing it.
- No serde round-trip tests for the large `messages.rs` type set; the
  real round-trip coverage lives in `aj-models/tests/roundtrip/`. That's
  an acceptable placement, but the SDK relies on a downstream crate for
  its own wire correctness — note for synthesis.

No wall-clock/network/filesystem coupling in the existing tests; no
flakiness risk. `dotenv`/`tokio` dev-deps are used only by the example,
not the tests.

## Cross-cutting themes to bubble up

- **Error-type consistency across provider SDKs.** Verify `openai-sdk`
  presents a single `thiserror` error contract and doesn't mix in
  `anyhow`. The `ClientError` shape (status + retry-after + accessors) is
  a good model to standardize on across both SDKs.
- **Dead/unused public surface on "thin" clients.** Both provider SDKs
  likely expose more than `aj-models` consumes (orphaned conversions,
  setters, never-constructed error variants). Synthesis should decide the
  intended surface policy (internal wire layer vs. general SDK) once and
  apply it to both.
- **`thiserror` vs. hand-written `Display`.** Check whether `openai-sdk`
  and `aj-models` error enums mix manual `Display` with the derive; pick
  one convention.
- **HTTP-boundary test coverage.** Neither SDK appears to test its own
  request/error mapping directly, relying on `aj-models` round-trip
  tests. Worth a consistent decision on where wire-correctness is
  asserted.
