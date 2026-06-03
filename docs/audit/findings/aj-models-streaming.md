# Audit findings — aj-models-streaming

- **Step:** M2
- **Date:** 2026-06-02
- **Audited commit:** 867a6df
- **Scope:** `src/aj-models/src/streaming.rs`, `transform.rs`,
  `partial_json.rs` (incl. the in-module `#[cfg(test)]` suites in all
  three).

## Summary

The streaming/transform layer is the strongest unit audited so far in
`aj-models`. The unified `AssistantMessageEvent` protocol is genuinely
provider-agnostic and exhaustively documented: every event carries an
owned `partial` snapshot, the terminal contract ("exactly one `Done` or
`Error`; pushes after terminal are dropped") is stated on the type,
enforced by an `AtomicBool`, and exercised by tests. The
`AssistantMessageEventStream` handle has a careful concurrency design
(notify-before-check in `result()`, race-safe `terminate` via
`swap`, single-consumer poll documented), and the abort path (`aborted`,
the synthesized "stream ended without a terminal event" error) gives the
agent loop a uniform, typed terminal message even when the producer
misbehaves. `transform.rs` is a clean two-pass rewrite with
spec-anchored rules and a large in-module + downstream test suite;
`partial_json.rs` is a small, well-documented best-effort parser with a
clear fallback ladder. Findings are localized: a generic cancellation
utility (`select_cancel`) parked in the streaming-protocol module, an
inherent O(n²) reparse on the tool-call hot path with an avoidable
per-delta `Vec<char>` allocation in `repair_json`, and a few minor
contract/comment gaps. No correctness, data-loss, or secret risks; the
boundaries hold.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 0 | 4 | 3 |

## Findings

### [Minor][Boundaries] `select_cancel` / `SelectOutcome` are a generic cancellation utility living in the streaming-protocol module — `src/aj-models/src/streaming.rs:211,229`
**What:** `streaming.rs`'s module doc scopes it to "the unified
`AssistantMessageEvent` / `AssistantMessageEventStream` protocol." But
`select_cancel` and `SelectOutcome` have nothing to do with that
protocol — they are a thin `tokio::select!` wrapper that races an
arbitrary future against a `CancellationToken`. They're used pervasively
outside the event types: by all four provider adapters' `run_stream_inner`
loops (`anthropic/provider.rs:168,180`, `openai/provider.rs:176,193`,
`openai/responses.rs:265,278`, `openai/codex.rs:219,242`) and even to race
a `tokio::time::sleep` in `scripted.rs:265`, where no `AssistantMessage`
is in sight.
**Why it matters:** Cohesion: the module's stated single responsibility
is the event protocol, but it also houses a general-purpose async helper.
A reader looking for cancellation plumbing won't expect it here, and the
helper's contract ("`None` token ⇒ just await") is documented in terms of
`StreamOptions::cancel`, coupling a generic utility to a specific caller.
**Suggested action:** Move `select_cancel`/`SelectOutcome` to a small
`cancel` (or `util`) module, or co-locate with `StreamOptions`. Keep the
re-export path the providers use. Mechanical; the only question is the
destination module, so confirm with the user.
**Effort:** S

### [Minor][Misc] Tool-call argument parsing is O(n²) across a stream, with an avoidable per-delta `Vec<char>` allocation in `repair_json` — `src/aj-models/src/partial_json.rs:34,91`
**What:** `parse_streaming_json` is called on the *cumulative* argument
buffer on every `ToolCallDelta` (e.g. `openai/provider.rs:1014`:
`slot.arguments.push_str(args); tc.arguments = parse_streaming_json(&slot.arguments)`;
same pattern in the other adapters). So a tool call streamed in N deltas
re-parses an ever-growing buffer N times — O(n²) in the final argument
length. The snapshot contract makes some reparsing inherent, but
`repair_json` (`partial_json.rs:91`) additionally does
`input.chars().collect::<Vec<char>>()` — a full heap allocation of the
cumulative buffer — on *every* non-strict-parse delta, even though it only
needs single-char lookahead. For large tool arguments (file writes, big
diffs) this is real work on the streaming hot path.
**Why it matters:** Opportunistic hot-path check from the rubric: needless
allocation/CPU in a streaming loop. Most deltas leave the buffer
mid-string/mid-bracket, so the strict parse at step 1 fails and the code
falls into `repair_json` (and its `Vec<char>`) every time.
**Suggested action:** Two independent wins: (1) rewrite `repair_json` to
iterate over `char_indices()` with a one-char peek instead of
materializing a `Vec<char>` (eliminates the per-delta allocation, no
behavior change); (2) consider whether callers can skip re-parsing until a
delta plausibly closes structure, or parse incrementally — but that
changes the snapshot cadence, so workshop it. The `repair_json`
allocation fix is the safe, contained part.
**Effort:** M

### [Minor][Contracts] `complete_partial_json` silently drops sibling keys when a value is a partial number/keyword — `src/aj-models/src/partial_json.rs:192`
**What:** `complete_partial_json` only closes strings/brackets and trims
trailing `,`/`:`. A buffer ending in a partial number or keyword (e.g.
`{"a": 1, "b": 1.}` mid-stream, or `{"ok": tru`) is not repaired, so the
strict parse of the completed string fails and `parse_streaming_json`
falls all the way through to the **empty object** (step 5). That discards
the already-complete `"a": 1` and any other sibling keys for that
snapshot, not just the in-flight value. The doc on `complete_partial_json`
notes it "Doesn't try to repair partial keywords … or partial numbers,"
but the downstream consequence — *the whole snapshot collapses to `{}`*,
not "the trailing value is omitted" — is not stated at the
`parse_streaming_json` boundary where callers reason about it.
**Why it matters:** A contract gap on the partial-parse boundary: a
caller rendering streamed tool args may see fully-parsed keys blink to an
empty object for the deltas where the tail is a partial number, then
reappear. Behaviorally tolerable (it self-corrects on the next delta) but
undocumented and surprising.
**Suggested action:** Add one sentence to `parse_streaming_json`'s doc:
"when the cumulative buffer ends in a partial number or keyword no
strategy succeeds, so the snapshot for that delta is the empty object
fallback (it recovers once the value completes)." Optionally extend
`complete_partial_json` to trim a trailing incomplete number/keyword token
so sibling keys survive. Doc-only is S; the trim is M.
**Effort:** S

### [Minor][Comments] `transform.rs` module doc and `signatures_portable` cite empirical/“other providers” behavior as fact without a verifiable anchor — `src/aj-models/src/transform.rs:100,107`
**What:** The `SIGNATURE_PORTABLE_PROVIDER` doc asserts Anthropic
signatures stay "valid across model-id changes within the Messages API
(empirically verified against the live API)" and that `openai-responses`
"rejects reasoning items across model boundaries with
`invalid_encrypted_content`." These are load-bearing claims — they decide
whether signed thinking is preserved or demoted — but they're asserted
prose with no test pinning the contract and no spec section cited at that
line (the rest of the module is meticulous about `§8.x` references).
**Why it matters:** Contracts/comments dimension: a future reader can't
tell whether this is still true or how it was established, and the
behavior it gates (preserve vs. demote) has no direct unit test for the
"openai-responses cross-model" demotion path beyond the generic
cross-model test (which uses `openai-completions`).
**Suggested action:** Anchor the claim to the spec section that records it
(`§8.1`) and/or add a focused test asserting that an `openai-responses`
source → different `openai-responses` model demotes/drops thinking
(i.e. `signatures_portable` returns false off-Anthropic). Keeps the
"why" knowledge without relying on un-anchored prose.
**Effort:** S

### [Nit][Comments] `truncate` doc says "at most `max` characters … slicing on UTF-8 boundaries" but the body slices bytes — `src/aj-models/src/transform.rs:278`
**What:** `truncate` does `s.len() <= max` then `s[..max]` (byte length,
byte slice). The doc frames it as "at most `max` characters, slicing on
UTF-8 boundaries (safe here because `sanitize` strips non-ASCII)." For the
sanitized ASCII inputs every caller passes, bytes == chars and the byte
slice is on a boundary, so it's correct — but the doc conflates "bytes"
and "characters," and the safety argument is the *only* thing keeping the
byte slice from panicking on a non-boundary.
**Why it matters:** Minor precision: the invariant (callers must pass
sanitized/ASCII input) is real and load-bearing but stated as an aside.
A future caller that truncates un-sanitized input would hit a slice panic,
not silent mis-truncation.
**Suggested action:** Reword to "Truncate to at most `max` **bytes**.
Caller must pass ASCII input (post-`sanitize`); the byte slice would
panic on a non-ASCII boundary otherwise." Optionally `debug_assert!` the
input is ASCII.
**Effort:** S

### [Nit][Comments] `AssistantMessageEvent` per-event clone rationale is asserted, not measured — `src/aj-models/src/streaming.rs:68`
**What:** The doc says "Cloning per event is cheap relative to the network
cost of producing the deltas." For text/thinking deltas the partial grows
with the message, so each snapshot deep-clones the whole accumulated
`content` — O(n²) total clone work over a long turn, mirroring the
partial-JSON reparse cost. The "cheap relative to network" framing is a
reasonable design call but is stated as settled fact.
**Why it matters:** Nit; the design (self-contained snapshots, no shared
mutable state) is defensible and the comment is honest about the
trade-off's *intent*. Flagging only because two independent O(n²) costs
(this clone + the JSON reparse) compound on the same hot path.
**Suggested action:** None required; if the partial-JSON perf finding is
acted on, note here that the snapshot clone is the other half of the
per-delta cost so the trade-off is evaluated as a whole.
**Effort:** S

### [Nit][Style] `partial_json.rs` strategy-chain doc duplicates the inline step comments — `src/aj-models/src/partial_json.rs:25`
**What:** The module/function doc enumerates the 5-step strategy chain,
and the function body repeats each step as an inline numbered comment
(`// 1. Strict parse.`, `// 2. Repair + strict parse…`, etc.). The two
lists must be kept in sync by hand.
**Why it matters:** Minor "fluff"/duplication: the inline comments mostly
restate the doc; if the chain changes, both lists need editing. The
inline notes that add *new* information (e.g. "skip if `repair_json` was a
no-op so we don't pay for a redundant parse") are worth keeping.
**Suggested action:** Trim the inline comments to only the non-obvious
notes, letting the function doc be the canonical chain description.
**Effort:** S

## What's good

- **Unified streaming event protocol (`streaming.rs:66`).** Provider-
  agnostic by construction: `AssistantMessageEvent` names content blocks
  by `content_index` and carries an owned `AssistantMessage` snapshot, so
  nothing about Anthropic SSE vs. OpenAI chunk shapes leaks into the
  protocol. The terminal contract is stated on the type, the
  `is_terminal`/`partial` accessors are exhaustive over the variants, and
  `DoneReason`/`ErrorReason` are deliberate `StopReason` subsets with
  `From` conversions — the "successful terminations are only
  Stop/Length/ToolUse" invariant is encoded in the type, not by
  convention.
- **Abort/terminal handling (`streaming.rs:174,370`).** `aborted` stamps
  `StopReason::Aborted`, preserves all accumulated deltas verbatim, and
  only synthesizes an `Aborted`-category error when none was present — a
  well-documented, idempotent constructor. `terminate` is race-safe
  (`swap(true)` gate), populates `final_message` only if missing,
  drops the sender to close the channel, and wakes `result()` waiters. The
  "stream ended without a terminal event" path synthesizes a *typed*
  transient error so the agent loop never sees a silent close. All of
  this is covered by tests (terminal drop, end-without-terminal,
  end-after-terminal, result resolution).
- **`result()` notify-before-check (`streaming.rs:355`).** Subscribes to
  the `Notify` *before* inspecting the message slot, so a wakeup that
  races the check isn't missed — the classic correct pattern, and the
  comment says exactly why. The single-consumer poll contract is
  documented and the `expect` on the receiver mutex is justified (poison =
  programmer error).
- **Two-pass transform (`transform.rs:66`).** Cohesive, spec-anchored
  (`§8.1` rules cited per branch), never mutates input. Pass 1 normalizes
  assistant content and builds the tool-call ID map; pass 2 consumes the
  map to align/synthesize/drop tool results. The same-model fast path and
  the `signatures_portable` Anthropic-only preservation are both
  documented with their rationale. `normalize_tool_call_id` handles the
  composite-ID / foreign-origin / `fc_` cases explicitly and has a
  thorough per-case test matrix.
- **Placeholder constants (`transform.rs:31-52`).** The four image
  placeholders and the orphan-result text are public, documented as
  "downstream consumers may match against these exact values," and split
  non-vision vs. blocked-by-config so the two failure modes are
  distinguishable in transcripts — the right call for a publicly-observable
  string contract.
- **`partial_json.rs` fallback ladder.** Small, single-responsibility,
  never panics, never returns `null`, always returns a value — the
  streaming-tool-call invariant is stated and the empty-object fallback
  is the documented floor. The strict→repair→complete→both→empty escalation
  is clear, and the "skip repair re-parse when it was a no-op" optimization
  is the kind of non-obvious note worth a comment.

## Boundary & architecture notes

Dependency direction is correct: all three modules depend only on
`std`, external crates (`futures`, `tokio`, `tokio-util`, `serde`,
`serde_json`, `chrono`) and in-crate `crate::types` / `crate::registry`
— no `aj_*` edges, so the unit sits below `aj-agent`/`aj-session` as
`CLAUDE.md` intends. The provider adapters (M3/M4) are the consumers that
build the `partial` and push events; the accumulation loop itself lives
there, not here — this unit defines the contract and the parser, which is
the right split.

Public-surface observations for synthesis:
- `select_cancel`/`SelectOutcome` (Minor finding) are a generic
  cancellation utility miscategorized into the streaming-protocol module;
  they're public and used by every provider plus `scripted.rs`.
- `repair_json` and `complete_partial_json` are `pub` but only consumed
  by `parse_streaming_json` within the crate (grep shows no external
  caller). They're tested directly, which justifies the visibility, but
  they could be `pub(crate)` if the direct unit tests moved to exercising
  `parse_streaming_json` — worth noting against the "dead/over-broad
  public surface" theme rather than acting on alone.
- `transform.rs`'s `transform_messages` / `block_user_images` and the
  placeholder consts are the legitimate public boundary, consumed by the
  providers and `aj-agent` and covered by `tests/roundtrip/cross_provider.rs`.

## Test assessment

Tests are in-module under `#[cfg(test)]` per convention and exercise the
public contract well:

- **streaming.rs** covers push-order delivery, drain/close,
  `Done`/`Error`/`aborted` resolution of `result()`, pushes-after-terminal
  drop, end-without-terminal synthesis (with a spawned waiter to verify
  the wakeup), end-after-terminal no-overwrite, and the `DoneReason`/
  `ErrorReason` → `StopReason` maps. This is good boundary testing of a
  concurrency-sensitive type. **Gap:** no test exercises two concurrent
  consumers (the documented "concurrent polls panic" contract) or a
  multi-event non-terminal stream that's then `aborted()` mid-flight; the
  abort constructor is unit-tested in providers (M3/M4) but the
  stream-level interaction isn't pinned here.
- **transform.rs** has a dense in-module suite plus the downstream
  `tests/roundtrip/cross_provider.rs` end-to-end matrix (all four
  supported directions, position-by-position assertions). Every §8.1 rule
  and §8.2 downgrade is covered, including the rule-5 + ID-normalization
  interaction and the adjacent-image-run collapse. `short_hash`
  determinism is pinned. Strong.
- **partial_json.rs** covers structural completion, the repair path
  (control chars, invalid escapes, dangling backslash, combined
  repair+complete), unicode-escape preservation, and the garbage→empty
  fallback. **Gap:** no test pins the partial-number/partial-keyword
  collapse-to-`{}` behavior (Minor finding) — the case where a
  fully-parsed sibling key is lost for one delta is neither documented nor
  tested.

Flakiness risk: `close_pending` (`transform.rs:387`) stamps synthetic
orphan results with `chrono::Utc::now().timestamp_millis()` — wall-clock
coupling, but the tests don't assert on that timestamp, so no flakiness
today. Worth noting the transform output is non-deterministic in that one
field (synthesis: persisted/replayed transcripts won't be byte-stable
across runs for synthesized orphans).

## Cross-cutting themes to bubble up

- **`async-stream` declared but unused (CONFIRMED again).** M1 flagged
  `async-stream.workspace = true` in `aj-models/Cargo.toml` as unused; the
  M2 streaming layer — the most plausible user — does not touch it either
  (`Stream` is hand-implemented on `AssistantMessageEventStream`, channels
  via `tokio::sync::mpsc`). This closes the "verify against the streaming
  step before removal" caveat from M1: it is safe to remove.
- **Over-broad public surface (NEW locus).** `repair_json` /
  `complete_partial_json` are `pub` with only an in-crate caller — same
  "dead/over-broad public surface" theme the SDK/M1 reports raised, here
  as helpers that could be `pub(crate)`. `select_cancel`'s misplacement is
  a cohesion variant of the same.
- **Compounding O(n²) on the streaming hot path (NEW).** Two independent
  per-delta costs stack on streamed tool calls: the full-buffer
  `parse_streaming_json` reparse (+ `repair_json`'s `Vec<char>` alloc) and
  the deep `partial` clone carried by every `AssistantMessageEvent`.
  Neither is wrong given the snapshot contract, but synthesis should note
  the streaming pipeline as the one place where allocation discipline
  matters most.
- **Wall-clock in transform output (NEW, minor).** Synthetic orphan tool
  results use `Utc::now()`, so transform output isn't deterministic in the
  timestamp field — relevant when comparing/replaying transcripts. Pairs
  with whatever the session-replay (SE1) audit finds about timestamp
  stability.
