# Audit findings — aj-models-openai

- **Step:** M4
- **Date:** 2026-06-02
- **Audited commit:** d93f242
- **Scope:** `src/aj-models/src/openai.rs`,
  `src/aj-models/src/openai/provider.rs` (Chat Completions),
  `src/aj-models/src/openai/responses.rs` (Responses),
  `src/aj-models/src/openai/codex.rs` (Codex Responses), all three with
  their in-module `#[cfg(test)]` suites, plus
  `tests/roundtrip/openai_completions.rs`,
  `tests/roundtrip/openai_responses.rs`,
  `tests/roundtrip/openai_codex_responses.rs`, and
  `tests/roundtrip/common.rs` (shared harness, for context).

## Summary

The three OpenAI adapters are a clean, well-documented translation
between the unified wire types and the `openai-sdk` types, and the
Responses↔Codex sharing is genuinely good design: Codex reuses the §7.3
`StreamState` wholesale through an injected api-name + cost-multiplier,
and only owns its auth/JWT, header, request-shape, and event-
normalization deltas. The SDK owns SSE framing in all three paths, so
the "duplicated SSE logic" theme is **refuted** for the streaming
framing. However the **truncation-looks-like-success** gap flagged in
M3 holds across all three OpenAI providers, and is sharper here: a
stream that ends on `SelectOutcome::Ready(None)` is finalized with a
`None` finish_reason / status that every classifier maps to a clean
`Done(Stop)` — a dropped connection mid-turn reads as a complete answer.
Beyond that: the per-provider `classify_client_error` is duplicated
verbatim (provider.rs ↔ responses.rs, codex wrapping responses'), the
roundtrip suites are happy-path-only (no error/truncation/abort
fixture), the same over-broad test-only `pub` surface recurs, and Codex
classifies a terminal `response.failed` differently from Responses (hard
`Err` short-circuit vs. in-state `Error` finalize) without the
divergence being called out. No secret leaks; the JWT account-id is
decoded at request time and only forwarded as a header.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 2 | 5 | 3 |

## Findings

### [Major][Errors] Stream closing without a terminal event is finalized as a successful `Done(Stop)` across all three providers — `src/aj-models/src/openai/provider.rs:207,1214`, `responses.rs:285,1472`, `codex.rs:261`
**What:** Every provider's `run_stream_inner` `break`s out of the poll
loop on `SelectOutcome::Ready(None)` (the SSE stream ended) and then
calls `state.finalize()`. In Chat Completions, `finalize` →
`classify_finish(&None)` returns `(StopReason::Stop, Some(DoneReason::Stop),
None)` (`provider.rs:1214`). In Responses/Codex, `finalize` →
`classify_status(None, …)` hits the `Some(Completed) | None => Stop`
arm (`responses.rs:1472`). So a stream that ends *before* its terminal
frame — `finish_reason` chunk for Completions, `response.completed` /
`response.failed` for Responses/Codex — is reported to the agent as a
clean `Done` with whatever partial content had accumulated. The
`openai-sdk` SSE filter ends the stream on a transport-level error by
yielding `ClientError::InternalError` *or*, for an upstream-truncated
body / `[DONE]`-less close, simply by the byte stream ending — the
latter surfaces here as `Ready(None)`, not `Ready(Some(Err))`. The
Completions path even logs the case (`provider.rs:216`,
`"stream closed without finish_reason; finalizing"`) and proceeds to
`Done` anyway; Responses/Codex don't log at all.
**Why it matters:** This is the same inverse-of-the-terminal-contract
bug M3 found in the Anthropic adapter, confirmed across all three
OpenAI surfaces. A partial assistant turn that should be retried (a
transport hiccup that drops the connection after some deltas but before
the terminal frame) is accepted as final and complete, so the agent
loop won't retry and the user silently gets a truncated answer with
`StopReason::Stop`. It also under-counts cost/usage silently because the
usage-bearing terminal frame never arrived.
**Suggested action:** Treat stream-end-without-terminal as a retryable
transient error: track a `saw_terminal` flag (Completions already has
one at `provider.rs:190` but ignores it; Responses/Codex have none) and,
when it's `false` on `Ready(None)`, finalize into an `Error` event
(transport/Transient category) rather than `Done`. Decide the exact
category with the user alongside the M3 Anthropic fix so a single
cross-provider policy lands. (Note: Completions's `ProcessOutcome.terminal`
is *always* `false` — see `provider.rs:799` — so its `saw_terminal` only
ever flips via the unused field; the real signal is "did we see a
`finish_reason`".)
**Effort:** M

### [Major][Simplicity] `classify_client_error` is duplicated verbatim across the Completions and Responses providers (and wrapped a third time by Codex) — `src/aj-models/src/openai/provider.rs:236`, `responses.rs:311`, `codex.rs:291`
**What:** `provider.rs:236-253` and `responses.rs:311-328` are
byte-for-byte identical: the same four-arm match over `ClientError`
fanning out to `classify_openai_error` / `transport_error`. Codex's
`classify_codex_client_error` (`codex.rs:291-318`) is the same four arms
again with only the `ApiError` arm overlaying the friendly-429 message.
This is exactly the within-crate duplication the `openai-sdk` audit
predicted would recur at the provider layer (its cross-cutting note:
"check whether aj-models' provider layer has the same per-endpoint
duplication"). The Anthropic adapter avoids this by having a single
`classify_client_error` fan-out; here it's copied per module.
**Why it matters:** Three copies of the `ClientError` → `AssistantError`
mapping drift independently — a fix to one (e.g. a new `ClientError`
variant, or changing how `ParseError` is categorized) can miss the
others. The transport/parse/internal arms are genuinely identical and
have no reason to differ between Completions and Responses.
**Suggested action:** Factor a single
`classify_openai_client_error(&ClientError) -> AssistantError` (e.g. in
`crate::errors` or a shared `openai` module helper) that all three call;
Codex layers its friendly-429 overlay on top of the shared `ApiError`
arm rather than re-spelling the transport/parse/internal arms.
**Effort:** S

### [Minor][Errors] Codex turns a terminal `response.failed` into a hard `Err` short-circuit while Responses finalizes it as an in-state `Error` — divergent terminal handling, undocumented — `src/aj-models/src/openai/codex.rs:446,451`, `responses.rs:1070`
**What:** In Responses, `ResponseStreamEvent::ResponseFailed` is handled
*inside* the state machine (`responses.rs:1070-1075`): it records
`finish_error`, sets `finish_status = Failed`, captures the response,
and the run continues to `finalize()` which emits an `Error` event
carrying the accumulated partial content + usage. In Codex,
`normalize_codex_event` intercepts both `Error` and `ResponseFailed`
*before* the state machine and returns `Err(...)` (`codex.rs:448-461`),
which `run_stream_inner` propagates so `run_stream` builds a *fresh*
empty `AssistantMessage` (`codex.rs:142-151`) — discarding any partial
content/usage accumulated in `state.partial`. The same divergence holds
for a mid-stream `ResponseStreamEvent::Error`: Responses keeps the
partial (`responses.rs:1076`), Codex drops it.
**Why it matters:** Two providers that share the §7.3 state machine
classify the same terminal-error wire shape into structurally different
unified messages (partial-preserving vs. empty), and nothing in the
module docs explains why Codex must short-circuit. A failed Codex turn
loses the streamed-so-far content and the usage that would have been
billed, where the equivalent Responses turn keeps it.
**Suggested action:** Either route Codex's terminal `error` /
`response.failed` through the same in-state path (forward to
`StreamState::process`, let `finalize` emit the `Error` with the
partial) so both providers agree, or document at `codex.rs:446` why the
short-circuit/partial-drop is intentional for this backend. Reconcile
with the §10.3 spec.
**Effort:** M

### [Minor][Testing] The three roundtrip suites exercise only happy-path parse/serialize — no error, truncation, abort, refusal, or content-filter scenario — `tests/roundtrip/openai_completions.rs:308`, `openai_responses.rs:247`, `openai_codex_responses.rs:281`
**What:** Completions has `text_only` / `tool_call` / `reasoning_text`;
Responses has `text_only` / `thinking_text` / `tool_call`; Codex adds a
`legacy_done` normalization case (the one genuinely provider-specific
fixture, nicely done). All run parse / serialize / semantic-roundtrip on
well-formed SSE that ends in a proper terminal frame. None of the
boundary's *error* paths is exercised through the public seam: a
`finish_reason: content_filter` / `network_error` chunk (Completions →
`Error`), a `ResponseStatus::Incomplete` with
`incomplete_details.reason` (Responses length/content-filter), a
`response.failed` / SSE `error` frame, a stream that ends *without* a
terminal frame (the Major finding above), or the Codex friendly-429
overlay. These error-mapping and terminal-classification quirks are
exactly what the adapters exist to contain, and they live only in
in-module unit tests that hand-build events — not pinned against
captured wire fixtures the way the happy path is.
**Why it matters:** The Testing rubric asks specifically for edge/error
coverage at the boundary. The truncation-as-`Done` Major finding sits in
a path no fixture touches; a "truncated SSE" fixture would have caught
it. Same happy-path-only pattern M3 flagged for Anthropic, confirmed for
all three OpenAI providers — the wire-correctness error legs are
under-fixtured workspace-wide.
**Suggested action:** Add fixtures + scenarios for: a Completions
`content_filter` finish, a Responses `incomplete` (length) and a
`response.failed`, a Codex 429 / `error` frame, and a
terminating-frame-missing SSE for each, asserting the terminal event
kind and `error.category`. The `replay_sse_events` helpers already
return the finalized message including the `Error` leg.
**Effort:** M

### [Minor][Boundaries] Round-trip helpers and the `TextSignatureV1` envelope are `pub` solely to serve the integration test crate — `src/aj-models/src/openai/provider.rs:577,610,673`, `responses.rs:94,745,759,867`, `codex.rs:647,660,676`
**What:** `assistant_message_to_request_item`,
`parse_assistant_request_item`, `replay_sse_events` (Completions);
`TextSignatureV1`, `assistant_message_to_input_items`,
`parse_assistant_input_items`, `replay_sse_events` (Responses);
`assistant_message_to_input_items`, `parse_assistant_input_items`,
`replay_sse_events` (Codex) are all `pub` with doc comments stating they
are "exposed publicly so the round-trip test suite … can reach it
directly." Grep confirms the only consumers are the
`tests/roundtrip/openai_*.rs` files; no non-test workspace code calls
them. This is the same over-broad-public-surface theme M1/M2/M3 raised,
here at triple the locus (nine functions + one struct across three
modules).
**Why it matters:** The crate's public API advertises ten provider-
internal projections as supported entry points purely because
integration tests can't see `pub(crate)`. It widens the contract surface
and blurs which functions are the real `Provider` boundary
(`stream`/`stream_simple`) versus test scaffolding. The internal
`pub(super)` helpers in `responses.rs` (used by `codex.rs`) are the
correct visibility; these `pub` ones are the leak.
**Suggested action:** Pick one workspace policy — feature-gate behind
`#[cfg(feature = "test-helpers")]`, `#[doc(hidden)]`, or move roundtrip
tests in-module — and apply it consistently with the M3 Anthropic
locus. Track in synthesis as one decision.
**Effort:** S

### [Minor][Contracts] `ProcessOutcome.terminal` is dead in the Completions provider — always `false`, yet drives the (ignored) `saw_terminal` flag — `src/aj-models/src/openai/provider.rs:792,880,199`
**What:** `ProcessOutcome` carries a `terminal: bool` field whose doc
says "Reserved for protocol-level terminators if any get added"
(`provider.rs:799`); `process` hardcodes it to `false`
(`provider.rs:880`). The poll loop reads it into `saw_terminal`
(`provider.rs:199-202`) which therefore can never become `true`, so the
`if !saw_terminal` log at `provider.rs:215` *always* fires on a clean
stream end and the `break` at line 201 is unreachable. The real
end-of-message signal (a non-null `finish_reason`) is captured into
`state.finish_reason` but never breaks the loop — the loop relies on the
server closing the stream after the usage chunk.
**Why it matters:** Dead field + a flag that's structurally always
`false` is the seam through which the Major truncation finding hides: a
reader sees `saw_terminal` and assumes it distinguishes clean vs.
truncated termination, but it doesn't. Fixing the Major finding will
likely repurpose this field, so it's worth resolving together.
**Suggested action:** Either remove `terminal` and `saw_terminal`
entirely (the loop ends on stream close regardless), or wire
`saw_terminal` to "saw a `finish_reason`" so it actually distinguishes a
clean terminus from a truncated one — which is precisely what the Major
finding needs.
**Effort:** S

### [Minor][Comments] `finalize`'s "drop the close-block events on the floor" comment describes machinery the function doesn't use — `src/aj-models/src/openai/provider.rs:1109`
**What:** `StreamState::finalize` (Completions) calls
`self.close_open_blocks(&mut tail)` then `let _ = tail;`
(`provider.rs:1109-1117`) with a five-line comment explaining it keeps
the events "only if the stream ended abruptly, in which case … we emit a
synthetic `Done` directly instead." But the events are unconditionally
discarded (`let _ = tail`), and there is no synthetic-`Done`-with-blocks
branch — the close events are dropped in every case. On the abrupt-end
path this means any block left open when the stream truncates is closed
silently with no `*End` event reaching the consumer (the snapshot in
`partial.content` is still complete, so the final message is correct,
but the per-block event contract isn't honored on that path).
**Why it matters:** Comment hygiene + a latent contract gap: the comment
narrates a behavior ("emit synthetic Done directly") that isn't in the
code, and the on-the-floor drop means the "callers see a balanced
Start/End per block" invariant the streaming layer upholds elsewhere can
be violated on truncation. Reads as a botched edit.
**Suggested action:** Either flush `tail` to the consumer before
finalizing (so truncated streams still get their `*End` events), or
shorten the comment to state plainly that the close events are
intentionally dropped because the terminal message snapshot already
carries the complete content. Tie to the Major truncation fix.
**Effort:** S

### [Nit][Testing] Codex's `minutes_until` couples a unit test to wall-clock time — `src/aj-models/src/openai/codex.rs:391,1056`
**What:** `minutes_until` computes `resets_at - SystemTime::now()` and
rounds to minutes (`codex.rs:391-404`). The test
`friendly_message_for_usage_limit_with_plan_and_resets_at_envelope`
(`codex.rs:1056`) builds `resets_at = now + 17*60` and asserts the
message "contains `16 min` || `17 min`" to tolerate the rounding
boundary. It's a real wall-clock dependency: under load or a slow CI
runner the elapsed seconds between building `now_secs` and computing the
delta could push it below the tolerance window, and the production
function's output is inherently non-deterministic.
**Why it matters:** The Testing rubric flags hidden wall-clock coupling
as a flakiness risk. The ±1-minute tolerance papers over it but doesn't
eliminate it; the function isn't testable deterministically.
**Suggested action:** Factor a `minutes_until_at(resets_at, now_secs)`
that takes `now` as a parameter (production caller passes
`SystemTime::now`), and test the pure function with a fixed `now`.
Recurring wall-clock-non-determinism theme.
**Effort:** S

### [Nit][Simplicity] `normalize_response_status` is an identity function kept only "for symmetry with the spec text" — `src/aj-models/src/openai/codex.rs:578`
**What:** `normalize_response_status(response) -> Response` returns its
argument unchanged (`codex.rs:578-582`); its own doc comment admits
"Today our `ResponseStatus` enum already enumerates exactly that set …
any `ResponseStatus` we *do* hold is already in the recognized set. The
function exists for symmetry with the spec text." It's called from
`normalize_codex_event` (`codex.rs:467,479`) on the typed path, while the
*untyped* `Other`/legacy path does the real work via
`normalize_response_status_in_value` (`codex.rs:548`).
**Why it matters:** A no-op function with two call sites is needless
indirection; a reader has to chase it to discover it does nothing. The
real normalization is value-level and already lives elsewhere.
**Suggested action:** Inline the (no-op) typed calls and drop
`normalize_response_status`, leaving a one-line comment at the call sites
that typed `ResponseStatus` is already within the recognized set. If the
SDK enum later gains a catch-all, reintroduce it then.
**Effort:** S

### [Nit][Comments] `tool_calls` HashMap is keyed by `i32` wire index with a sort-on-finalize, but the contract that indices are unique-and-stable isn't stated — `src/aj-models/src/openai/provider.rs:775,1079`
**What:** `StreamState.tool_calls: HashMap<i32, ToolCallSlot>` keys on
the wire `tool_calls[i].index` (`provider.rs:775`), and
`close_open_blocks` drains + sorts by that index to emit `ToolCallEnd`
deterministically (`provider.rs:1079-1080`). The reliance on the wire
index being a stable, per-call-unique key (so two deltas for the same
call always land in the same slot, and distinct calls never collide) is
load-bearing but only implied. Unlike the Anthropic adapter's `index`
handling (which M3 flagged for an `expect`), this path is defensively
total (`.entry(...).or_insert_with`), so no panic — just an undocumented
invariant.
**Why it matters:** Minor contract-doc gap; the code is correct for
well-formed OpenAI streams but a reader can't tell whether a duplicated
or reused index would corrupt state (it would merge two logical calls).
**Suggested action:** One line on the `tool_calls` field: "keyed by the
wire `tool_calls[i].index`, which OpenAI guarantees stable and unique
per streamed call; deltas for the same index accumulate into one slot."
**Effort:** S

## What's good

- **Responses↔Codex sharing is the right abstraction (`codex.rs:57`,
  `responses.rs:964`).** Codex reuses the entire §7.3 `StreamState` via
  `new_with(api_name, model, tier, cost_multiplier)` — a tidy seam that
  injects exactly the two things that differ (api identity, pricing
  curve) and shares everything else (the state machine, reasoning
  round-trip, composite tool-call IDs, usage parsing, stop-reason
  mapping). The `CostMultiplierFn` function-pointer indirection is
  minimal and well-justified. This is the model the duplicated
  `classify_client_error` (Major) should follow.
- **SDK owns SSE framing; adapters own mapping.** All three providers
  consume `client.*_stream(...)` returning a decoded event stream and
  never touch `data:`/`eventsource` framing — so the "duplicated SSE
  logic across endpoints" theme is refuted at the framing level for this
  unit, matching the Anthropic conclusion.
- **Stateless `Provider` impls.** All three `stream`/`stream_simple`
  clone inputs and spawn; per-call auth/base-URL/headers/tier derive
  from `ModelInfo` + `StreamOptions`, so one instance serves concurrent
  requests — the lifecycle the trait doc promises.
- **Forward-compat event handling in Responses (`responses.rs:1003`).**
  `process` matches exhaustively over `ResponseStreamEvent`; web-search
  and `Other(_)` events are explicit no-ops, unknown output-item kinds
  (`WebSearchCall`, `Other`) are slotted away, and a delta for an unknown
  `output_index` early-returns instead of panicking — a total, defensive
  state machine.
- **Codex event normalization is careful (`codex.rs:446`,
  `rewrite_legacy_done:514`).** Legacy `response.done` /
  `response.incomplete` arriving as `Other` are detected by `type` and
  rewritten through serde so they re-fire as the typed terminal variant;
  on rewrite failure it `Forward`s the original rather than dropping
  terminal information ("better to feed the state machine an unknown
  event than to silently lose terminal information"). The `legacy_done`
  roundtrip fixture + `legacy_done_terminator_normalized` test pin this
  precisely.
- **Provider-specific quirks well-contained with spec anchors.** The
  Completions developer-vs-system routing (`provider.rs:363`), `store:
  false` + `include_usage` (`provider.rs:314,333`), Responses
  session-correlation-headers-only-on-openai-host (`responses.rs:247`),
  Codex JWT-at-request-time + `chatgpt-account-id` header
  (`codex.rs:181`), and the `gpt-5.5 + priority` 2.5× exception
  (`codex.rs:606`) are each isolated in a small helper with a §-anchored
  comment and a focused unit test.
- **Usage cache arithmetic is documented and tested (`provider.rs:1222`,
  `responses.rs:1527`).** The cache-write-subtracted-from-cached
  reconciliation for OpenAI-compatible providers, and the Responses
  "no cache writes reported" note, are both spelled out and pinned by
  `streamstate_usage_*` tests.
- **Secret handling is clean.** The Codex OAuth JWT is decoded only to
  extract the `chatgpt_account_id` claim and forwarded as a header; it's
  never logged or persisted. `on_payload` serialization failures log a
  `warn` with the error, not the body.

## Boundary & architecture notes

Dependency direction is correct: the adapters depend on `openai-sdk`,
`futures`, `serde`/`serde_json`, `base64` (Codex test only), and the
in-crate `errors`/`oauth`/`partial_json`/`provider`/`registry`/
`streaming`/`transform`/`types` modules — no `aj_*` edges, so the unit
sits below `aj-agent`/`aj-session` as `CLAUDE.md` intends, and
`openai-sdk` is consumed only here (and not by the M1 core). `openai.rs`
is a thin re-export shim (three `pub mod` + three `pub use`), which is
fine. The `codex.rs` → `responses.rs` intra-module dependency
(`use super::responses::{...}`) is the correct direction for the shared
state machine.

Public-surface note for synthesis: ten `pub` items across the three
modules exist only for integration-test access (Minor finding). This is
the recurring over-broad-public-surface theme at its widest locus so
far. The `pub(super)` helpers in `responses.rs` that Codex consumes are
correctly scoped; only the test-facing ones leak.

`responses_cost_multiplier`'s first parameter `_model_id` is unused
(`responses.rs:469`) — the public-API curve has no per-model exception,
only Codex does. Fine, but worth noting it exists solely to satisfy the
`CostMultiplierFn` signature shared with Codex.

## Test assessment

In-module unit tests are strong on the construction side and good on the
happy-path streaming side across all three providers: request building
(developer/system routing, store/include_usage, reasoning effort &
include, prompt-cache key/retention, Codex instructions routing &
hardcoded fields), tool-choice mapping, message/tool-result conversion,
usage cache arithmetic, finish-reason / status classification, the
Codex friendly-429 overlay, event normalization (legacy done/incomplete,
top-level error, unknown-Other forwarding), and two end-to-end
`#[tokio::test]` auth-error paths for Codex. The cost-multiplier curve
(including the `gpt-5.5` exception and server/requested fallback) is well
covered.

The three integration roundtrip suites exercise the real unified↔wire
boundary through the same public parse/serialize entry points the live
provider uses, with golden-JSON comparison and a single canonical source
of truth per scenario — genuinely testing the boundary, not internals.
The Codex `legacy_done` scenario + its dedicated normalization assertion
is the standout: a genuinely provider-specific edge pinned at the
boundary. The shared `common.rs` harness (SSE parser, fixture loader,
content-equality helper) is readable, documented, and self-tested.

Gaps (see findings): the roundtrip suites are happy-path-only — no
content-filter / network-error finish (Completions), no incomplete /
failed status or mid-stream error frame (Responses/Codex), no
terminating-frame-missing fixture (the Major truncation bug sits here),
no abort/cancel scenario. Codex's `minutes_until` test is wall-clock
coupled (Nit). No real-network coupling; `replay_sse_events` keeps the
HTTP client out of the loop, so no transport flakiness.

## Cross-cutting themes to bubble up

- **Stream-end-without-terminal classified as success (CONFIRMED across
  all three OpenAI providers).** The M3 Anthropic finding recurs
  verbatim in Completions, Responses, and Codex — all break on
  `Ready(None)` and `finalize` maps the missing terminus to `Done(Stop)`.
  This is now a four-provider pattern; synthesis should drive one policy:
  stream closing before its terminal frame → retryable `Error`, not
  `Done`. The `producer.end()` safety net only prevents a hang, it does
  not fix the misclassification.
- **Duplicated error-mapping within the crate (CONFIRMED, as the
  openai-sdk audit predicted).** `classify_client_error` is byte-
  identical between Completions and Responses and re-spelled a third time
  in Codex. Mirrors the openai-sdk's own duplicated streaming
  error-mapping. Factor once.
- **Over-broad public surface for test access (CONFIRMED, widest
  locus).** Ten `pub` items across three modules exist only for the
  integration-test crate — same theme as M1/M2/M3. One workspace policy
  needed.
- **Happy-path-only roundtrip coverage (CONFIRMED, recurring).** Three
  more provider suites that cover only the success path; error /
  truncation / abort legs under-fixtured. The Codex `legacy_done`
  scenario shows the suite *can* express provider-specific edges — the
  error legs just aren't written yet.
- **Wall-clock non-determinism (CONFIRMED, new locus).** Codex's
  `minutes_until` reads `SystemTime::now()` inside the production path
  and is tested with a ±1-minute tolerance. Factor the clock out, as the
  general pattern for the workspace.
- **Divergent terminal-error handling between sibling providers (NEW).**
  Codex drops the partial on `response.failed`/`error`; Responses keeps
  it. Worth a one-line policy decision so providers sharing a state
  machine agree on what a failed turn's unified message contains.
