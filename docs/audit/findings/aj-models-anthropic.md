# Audit findings — aj-models-anthropic

- **Step:** M3
- **Date:** 2026-06-02
- **Audited commit:** b440134
- **Scope:** `src/aj-models/src/anthropic.rs`,
  `src/aj-models/src/anthropic/provider.rs` (incl. its in-module
  `#[cfg(test)]` suite), `tests/roundtrip/anthropic.rs`, and
  `tests/roundtrip/common.rs` (shared harness, for context).

## Summary

The Anthropic adapter is a clean, well-structured translation between the
unified wire types and the `anthropic-sdk` types. The `Provider`
implementation is genuinely stateless, the SSE→event state machine is
cohesive and total over the SDK's event/block enums (unknown kinds are
explicitly slotted as `Ignored` rather than dropped silently), and the
provider correctly delegates all SSE framing/decoding to the SDK rather
than re-implementing it — so the "duplicated SSE logic" theme does *not*
hold here. The standout weakness is a correctness gap at the terminal
boundary: when the SSE stream closes *without* a `message_stop` (e.g. a
mid-stream transport error, which the SDK surfaces by ending the stream
after logging), the adapter finalizes the partial as a successful `Done`
rather than an `Error`, so a truncated turn looks like a clean stop to the
agent loop. The other findings are localized: the roundtrip suite is
serialize/parse-only and exercises no error, abort, or multi-block-index
edge case; `index` conversions use `expect` on a reachable wire value; a
duplicated doc-comment block; and the same `anyhow`-adjacent /
over-broad-public-surface threads the earlier M-steps raised (here:
`provider`-module functions exposed `pub` solely for the test crate).
Boundaries hold; the terminal-classification gap is the one thing worth
fixing before relying on the retry layer.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 4 | 3 |

## Findings

### [Major][Errors] SSE stream closing without `message_stop` is finalized as a successful `Done`, masking a truncated/errored turn — `src/aj-models/src/anthropic/provider.rs:191,202,1155`
**What:** `run_stream_inner` breaks out of the poll loop on
`SelectOutcome::Ready(None)` (the SSE stream ended) with `saw_terminal`
still `false`, logs a `debug!` line, and then calls `state.finalize()`
(`provider.rs:202-206`). `finalize` maps a `None` `stop_reason` to
`(StopReason::Stop, Some(DoneReason::Stop))` (`provider.rs:1155-1159`), so
the adapter emits `AssistantMessageEvent::Done`. But the SDK's
`messages_stream` ends the stream after merely logging on a transport-layer
event-stream error (`anthropic-sdk/src/client.rs:290-293`,
`tracing::error!("event-stream error: {e}")` → `None`), and likewise on an
upstream-truncated body. So a connection dropped mid-turn — after the
content blocks but before `message_stop` — is reported to the agent as a
clean `Done` with `StopReason::Stop`, not a retryable transient error.
**Why it matters:** This is the inverse of the careful "callers always see
exactly one terminal event" contract the rest of the stack upholds. A
partial assistant turn that should be retried (transport hiccup) is instead
accepted as final and complete, so the agent loop won't retry and the user
silently gets a truncated answer. It also means the unified terminal
contract — `Done` only for genuine `Stop`/`Length`/`ToolUse` — is violated
on the truncation path. The OpenAI adapters share `select_cancel`/the same
loop shape, so this is worth checking there too (M4).
**Suggested action:** Treat stream-end-without-`message_stop` as a
transient error: when `saw_terminal == false`, finalize into an
`Error` event (transport/`Aborted`-style category, retryable) rather than
`Done`, or thread the SDK's stream-error out of `messages_stream` as a
typed item instead of swallowing it. Decide the exact category with the
user since it spans the SDK seam.
**Effort:** M

### [Minor][Testing] The roundtrip suite exercises only happy-path parse/serialize — no error, abort, refusal, ignored-block, or stream-truncation scenario — `tests/roundtrip/anthropic.rs:267`
**What:** The four scenarios (`text_only`, `thinking_text`, `tool_call`,
`redacted_thinking`) each run parse / serialize / semantic-roundtrip, all
on well-formed SSE that ends in `message_stop`. None of the boundary's
*error* paths is exercised through the public seam: a mid-stream
`ServerSentEvent::Error` frame (→ `Error` event, classified category), a
`stop_details: refusal` `message_delta` (→ `ContentFilter` error), a
`model_context_window_exceeded` / `compaction` stop reason, an SSE dump
that ends *without* `message_stop` (the Major finding above), an
out-of-order / skipped `content_block` index (`pad_blocks_to`), or an
`Ignored` server-tool block interleaved with real content. These are the
provider-specific quirks the adapter exists to contain, and they live only
in in-module unit tests that hand-build `ServerSentEvent`s — they're not
pinned against captured wire fixtures the way the happy path is.
**Why it matters:** The Testing rubric asks specifically for edge/error
coverage at the boundary, not just the happy path. The error-mapping and
abort/truncation logic is exactly where wire-correctness bugs bite, and the
golden-fixture mechanism that catches "fixture and code drifted" for text /
thinking / tool calls covers none of it. The Major finding above would have
been caught by a "truncated SSE" fixture.
**Suggested action:** Add fixtures + scenarios for: an `error` SSE frame, a
refusal `message_delta`, and a stream that ends without `message_stop`,
asserting the resulting terminal event kind and `error.category`. Reuse
`replay_sse_events`, which already returns the finalized message including
the `Error` leg.
**Effort:** M

### [Minor][Contracts] `usize::try_from(index).expect(...)` panics on a reachable wire value — `src/aj-models/src/anthropic/provider.rs:905,985,1051`
**What:** All three `content_block_*` arms convert the SDK's `index`
(decoded from the wire) with
`usize::try_from(index).expect("... fits in usize")`. The value originates
from untrusted server JSON; on a 32-bit target, or a malformed/hostile
frame carrying a huge or negative-encoded index, this panics inside the
spawned stream task. Every *other* malformed-input path in this file is
handled defensively (unknown block kinds → `Ignored`, missing block slot →
early-return `ProcessOutcome`, `pad_blocks_to` tolerates gaps), so the
`expect` is the one spot that trades the file's otherwise-total handling
for a panic.
**Why it matters:** A panic in the spawned task aborts the stream future
without pushing a terminal event; the `producer.end()` safety net in
`spawn_stream` (`provider.rs:91`) would still fire and synthesize the
"ended without terminal" error, so it's not a hang — but it's a reachable
`expect` on wire data, which the Contracts/Errors rubric flags, and the
panic discards the accumulated partial.
**Suggested action:** Replace the `expect` with the same defensive pattern
already used at `provider.rs:986` — `let Ok(idx) = usize::try_from(index)
else { return ProcessOutcome { events, terminal } }` (or clamp). Document
that an out-of-range index drops the block rather than panicking.
**Effort:** S

### [Minor][Boundaries] `provider`-module conversion/parse helpers are `pub` solely to serve the test crate — `src/aj-models/src/anthropic/provider.rs:492,519,580`
**What:** `assistant_message_to_request_item`,
`parse_assistant_request_item`, and `replay_sse_events` are `pub` (and
re-exported via `anthropic::provider`) with their doc comments stating they
are "exposed publicly so the round-trip test suite … can call it directly"
and "Provided publicly so external test suites can share the exact same
parse path." Grep confirms the only consumer is
`tests/roundtrip/anthropic.rs`; no non-test code in the workspace calls
them. This is the same "over-broad public surface" theme M1/M2 raised
(`repair_json`/`complete_partial_json` `pub` with only in-crate callers),
here driven by integration-test access rather than a real API need.
**Why it matters:** The crate's public API advertises three provider-
internal projections as supported entry points purely because integration
tests (which can't see `pub(crate)`) need them. It widens the contract
surface the crate must keep stable and blurs which functions are the real
`Provider` boundary (`stream`/`stream_simple`) versus test scaffolding.
**Suggested action:** This is a known Rust integration-test tension; options
are (a) gate the three behind a `#[cfg(feature = "test-helpers")]` or
`#[doc(hidden)]` so they're clearly not part of the supported surface, or
(b) move the roundtrip tests in-module. Note in synthesis alongside the
M2 over-broad-surface finding so a single workspace policy is chosen.
**Effort:** S

### [Minor][Comments] `finalize` carries a duplicated, near-identical comment block — `src/aj-models/src/anthropic/provider.rs:1179`
**What:** Lines 1179-1184 contain two back-to-back comment paragraphs
saying the same thing: "Error-flavored terminal (e.g. refusal). Backfill an
error message if we don't already have one — callers should never see a
`StopReason::Error` without an accompanying message." immediately followed
by "Error-flavored terminal (e.g. refusal). Backfill a structured error if
we don't already have one — callers should never see a `StopReason::Error`
without an accompanying detail." One is a stale copy of the other.
**Why it matters:** Comment hygiene: a duplicated paragraph is noise and
signals a botched edit; a reader can't tell which is canonical.
**Suggested action:** Delete one of the two paragraphs (keep the
"structured error / detail" wording, which matches the code).
**Effort:** S

### [Nit][Comments] `MessageStart` "trust the wire value" comment overstates — server `model` always overrides registry id — `src/aj-models/src/anthropic/provider.rs:889`
**What:** The comment says "The server may report a slightly different
`model` than the registry id (e.g. version-pinned). Trust the wire value,"
and the code overwrites `self.partial.model` whenever
`message.model` is non-empty. That's fine, but the partial was already
stamped with `model.id` in `StreamState::new`; the nota-bene worth
recording is *why* the wire value wins (cost lookup keys off the registry
`ModelInfo`, not `partial.model`, so the override is display-only and can't
desync cost). As written it reads as a generic "trust the wire" without
noting that the cost path is unaffected.
**Why it matters:** Minor; the behavior is correct but the comment leaves a
reader wondering whether overwriting `model` could break `calculate_cost`
(it can't — `finalize_usage` takes `&self.model`).
**Suggested action:** Add half a sentence: "display-only; cost still keys
off the registry `ModelInfo`, so the override can't desync pricing."
**Effort:** S

### [Nit][Simplicity] `replay_sse_events`'s `last_event` tracking is more elaborate than the terminal contract requires — `src/aj-models/src/anthropic/provider.rs:585`
**What:** The helper walks every event, keeps `last_event` as the last
emitted event of the last processed frame, then after the loop checks
whether that happened to be an `Error` and returns its message; otherwise
it `finalize()`s and pattern-matches with a `panic!` on a non-terminal
result. Because `process` sets `terminal == true` exactly when it emits an
`Error` event *or* on `MessageStop`, the only frame whose last event can be
an `Error` is the terminal one, so the `last_event` bookkeeping is really
just "did the terminal frame carry an Error event." The trailing `panic!`
arm is unreachable (`finalize` only ever returns `Done`/`Error`).
**Why it matters:** Nit; the logic is correct but the indirection
(tracking the last event across all frames, then re-deriving terminality)
obscures the simple contract: "if a frame produced an `Error` event, return
its message; else finalize." The unreachable `panic!` is dead defensive
code.
**Suggested action:** Capture the `Error` message directly inside the loop
when `outcome.terminal` is set, and drop the `last_event` accumulator; or
leave as-is and downgrade the `panic!` to a comment that `finalize` is
total over terminal variants.
**Effort:** S

## What's good

- **SDK owns SSE; the adapter owns mapping (`provider.rs:167`,
  `anthropic-sdk/client.rs:249`).** `messages_stream` returns a
  `Stream<Item = ServerSentEvent>` of already-decoded, already-reverse-
  mapped events; the adapter never touches `eventsource`/`data:` framing.
  This cleanly refutes the "duplicated SSE logic across endpoints" theme
  *for this unit* — the framing lives once in the SDK, the semantic mapping
  lives once here.
- **Total, defensive state machine (`provider.rs:882`).** `process` matches
  exhaustively over `ServerSentEvent` and over `(BlockState, delta)` pairs;
  unknown block kinds become `BlockState::Ignored` with a populated content
  slot so indices stay aligned, a delta for an unknown index early-returns
  instead of panicking, and `pad_blocks_to` tolerates skipped indices. The
  `Ignored`/drop policy is the same on both the streaming and the
  `parse_assistant_request_item` paths, and both document the drop — no
  silent divergence between the two parse routes.
- **Stateless `Provider` impl (`provider.rs:46`).** `stream`/`stream_simple`
  clone their inputs and spawn; per-call auth/base-URL/betas/caching all
  derive from `ModelInfo` + `StreamOptions`, so one instance serves
  concurrent requests — exactly the lifecycle the trait's module doc
  promises.
- **Provider-specific quirks well-contained.** The temperature-vs-thinking
  mutual exclusion (`build_request:327`), the interleaved-thinking beta only
  on non-adaptive reasoning models (`build_client:241`), the 1h-cache-TTL
  direct-API-only fallback (`cache_control_for:652`), the
  `tool_choice: none`-with-no-tools omission (`to_anthropic_tool_choice:726`),
  and the adaptive-vs-budget thinking split (`build_thinking:734`) are each
  isolated in a small helper with a spec-anchored comment and a focused unit
  test. This is the right shape for keeping vendor quirks behind the seam.
- **Error classification is delegated, not duplicated (`provider.rs:277`).**
  `classify_client_error` is a thin fan-out into the shared
  `errors::classify_anthropic_*` / `transport_error` / `parse_retry_after`
  helpers audited in M1; the adapter adds only the
  `ClientError`→category mapping, and the mid-stream `Error` frame reuses
  the same `classify_anthropic_error` with `None` status (correctly noting
  SSE errors carry no HTTP status). No retry/status logic is re-implemented
  here.
- **`extra_betas_from_headers` (`provider.rs:257`).** Case-insensitive
  header match, comma-split, trim, empty-drop — fully specified in the doc
  and pinned by six unit tests covering each branch. A small, tidy boundary.
- **Roundtrip "single source of truth" design (`tests/roundtrip/anthropic.rs:92`).**
  Each scenario's `canonical_*` builder feeds both the parse assertion and
  the serialize golden, so a drift between fixture and code surfaces rather
  than being masked by two independent expected values — a good test-design
  pattern worth replicating. Splitting each scenario into three top-level
  tests (parse/serialize/semantic) so one failure doesn't mask another is
  also nicely done.

## Boundary & architecture notes

Dependency direction is correct: the adapter depends on `anthropic-sdk`
(the wire client), `futures`, `serde_json`, and the in-crate
`errors`/`partial_json`/`provider`/`registry`/`streaming`/`transform`/
`types` modules — no `aj_*` edges, so it sits below `aj-agent`/`aj-session`
as `CLAUDE.md` intends, and the SDK is consumed only here (and in M4),
never by the M1 core. `anthropic.rs` is a one-line re-export shim
(`pub mod provider; pub use provider::AnthropicProvider;`), which is fine.

Public-surface note for synthesis: three `pub` functions in
`provider.rs` (`assistant_message_to_request_item`,
`parse_assistant_request_item`, `replay_sse_events`) exist only to give the
integration test crate access to provider internals (Minor finding). That's
the recurring over-broad-public-surface theme, here driven by Rust's
integration-test visibility model rather than by a genuine external
consumer.

The `AStopReason::PauseTurn` → `StopReason::Stop` mapping
(`provider.rs:1157`) is worth a sanity check in synthesis: pause-turn is a
"server wants to continue" signal, and collapsing it to a plain `Stop`
means the agent treats a paused turn as a completed one. It's tested
(`provider.rs:1717`) so it's intentional, but the unified-spec rationale
isn't cited at that line.

## Test assessment

In-module unit tests are strong on the *construction* side and good on the
*happy-path* streaming side: request building (max_tokens default,
temperature-vs-thinking, cache-control placement), thinking config
(adaptive vs budget, display flag, xhigh fallback), tool-choice mapping,
message batching, usage merging/finalize/cost, and the text / tool-call /
redacted-thinking stream pipelines are all covered, plus a refusal→Error
finalize case. The header-beta parsing has a dedicated six-test matrix.

The integration roundtrip suite exercises the real unified↔wire boundary
through the same public parse/serialize entry points the live provider
uses, with golden-JSON comparison and a single canonical source of truth —
genuinely testing the boundary, not internals. The shared `common.rs`
harness (SSE frame parser, fixture loader, content-equality helper) is
readable, well-documented, and self-tested.

Gaps (see findings): the roundtrip suite is entirely happy-path — no
`error` frame, no refusal `message_delta`, no `model_context_window_exceeded`
/ `compaction` stop reason, no stream-truncation (`message_stop`-missing)
fixture, no out-of-order/skipped-index case, and no `Ignored`-block
interleaving. The error and abort legs are touched only by hand-built
`ServerSentEvent`s in unit tests, and the truncation-as-`Done` Major bug
sits squarely in an untested-at-the-boundary path. No wall-clock or
real-network coupling in any test; `replay_sse_events` keeps the live HTTP
client out of the loop, so no flakiness risk.

## Cross-cutting themes to bubble up

- **Duplicated SSE logic across endpoints (REFUTED for this unit).** The
  adapter delegates all SSE framing/decoding to `anthropic-sdk`'s
  `messages_stream` and only maps decoded `ServerSentEvent`s — no
  re-implementation. Synthesis should confirm the OpenAI adapters (M4) do
  the same against their SDK rather than re-rolling SSE.
- **Stream-end-without-terminal classified as success (NEW, important).**
  The truncated-stream-as-`Done` Major finding likely recurs in the OpenAI
  adapters (M4), which share the `select_cancel` poll-loop shape and the
  `producer.end()` safety net. Worth a cross-provider check and a single
  policy: a stream that closes before its terminal event should yield a
  retryable `Error`, not a `Done`.
- **Over-broad public surface for test access (CONFIRMED, new locus).**
  Three `provider.rs` functions are `pub` solely so the integration test
  crate can reach them — same theme as M1's `tools::Tool` and M2's
  `repair_json`/`complete_partial_json`. Synthesis should pick one workspace
  policy (feature-gated test helpers vs. `#[doc(hidden)]` vs. in-module
  tests).
- **Reachable `expect` on wire data (NEW).** The `index` →`usize`
  conversions `expect` on server-supplied values, against an otherwise
  uniformly defensive parser. A small, contained sweep candidate; check the
  OpenAI adapters for the analogous index/u32 conversions.
- **Happy-path-only roundtrip coverage (CONFIRMED, recurring).** As M1/M2
  noted that wire-correctness leans on the downstream `tests/roundtrip/`
  suite, this step shows that suite covers only the success path per
  provider — synthesis should note the error/abort/truncation legs are
  under-fixtured across the providers.
