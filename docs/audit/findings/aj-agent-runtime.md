# Audit findings — aj-agent-runtime

- **Step:** AG1
- **Date:** 2026-06-02
- **Audited commit:** 2f5dfd0
- **Scope:** `src/aj-agent/src/lib.rs`, `bus.rs`, `events.rs`,
  `projection.rs`, `hooks.rs`, `Cargo.toml` (incl. the in-module
  `#[cfg(test)]` suites: the `event_protocol_tests` module in `lib.rs`,
  and the `tests` modules in `bus.rs`, `events.rs`, `projection.rs`,
  `hooks.rs`).

## Summary

The agent runtime is a genuinely clean, single-owner state machine: one
`execute_turn` loop drives inference → tool batch → loop, every state
transition is mirrored as a typed `AgentEvent` on an in-process bus, and
persistence really is "just a subscriber" (the binary registers a
listener; the agent never touches a log). The bus is a small, correct,
inline-await broadcast with sound subscribe/unsubscribe semantics and a
deliberate "listener `Err` aborts the run" durability contract. Hooks are
well-scoped single-slot seams, not a god-hook. The cancellation handling
is the standout: a three-checkpoint design (pre-iteration, streaming
`select!`, per-tool `select!`) with a documented transcript-consistency
invariant and end-to-end tests.

The boundaries do **not** hold cleanly in two places. First, a stated
architecture rule is violated: `aj-agent` depends on `aj-conf`
(`AgentEnv`, `ConfigThinkingLevel`), but `CLAUDE.md` says the runtime
"depends only on aj-models." Second — the critical M3/M4 cross-check — the
runtime trusts the provider's terminal classification blindly: a `Done`
event is accepted as success with no guard, so the four-provider
truncation-looks-like-success bug surfaces straight through to the
transcript as a complete turn. The runtime *does* have its own
stream-closed-silently fallback (synthesizing a `Transient` error via
`result()`), but that branch is unreachable for the real truncation case
because the providers finalize the truncated stream as a `Done` before the
channel closes. Beyond that: three event variants (`TurnEnd`,
`QueueUpdate`, `ToolExecutionUpdate`) are declared but never emitted, the
`tools::Tool` round-trip (M1 Major) is confirmed crossing this boundary,
the manifest hardcodes `edition = "2021"` against a 2024 workspace, and
several comments narrate `docs/aj-next-plan.md` "§N lands later"
chronology.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 1 | 3 | 4 | 3 |

## Findings

### [Critical][Boundaries] `aj-agent` depends on `aj-conf`, violating the stated "depends only on aj-models" rule — `src/aj-agent/Cargo.toml:8`, `src/aj-agent/src/lib.rs:21`
**What:** `CLAUDE.md`'s architecture section states the dependency graph
`aj-models ← aj-agent ← aj-tools` and describes `aj-agent` as "the `Agent`
runtime … depends only on aj-models." The manifest declares
`aj-conf = { path = "../aj-conf" }` (`Cargo.toml:8`) and the runtime
consumes it directly: `use aj_conf::{AgentEnv, ConfigThinkingLevel}`
(`lib.rs:21`), with `AgentEnv` stored as an `Agent` field (`lib.rs:48`),
flowing through `with_provider`, `assemble_system_prompt` (`lib.rs:422`),
the `SessionContextWrapper` (`lib.rs:1561`), and `ConfigThinkingLevel`
mapped to `ThinkingConfig` in `with_provider` (`lib.rs:185-192`). So the
real edge `aj-agent → aj-conf` is not in the intended graph at all.
**Why it matters:** This is exactly the architecture-level boundary
violation the severity taxonomy calls Critical: a dependency edge the
graph doesn't mention, pulling a config-file-loading crate (`toml`,
`toml_edit`, `strsim`) into the runtime. `AgentEnv` (working dir, git
root, OS, context files) and the `ConfigThinkingLevel`→`ThinkingConfig`
remap are host/config concerns; the runtime conflating them with the
inference loop means a host that wants the `Agent` without `aj-conf`'s
config model can't have it, and the "minimal runtime over aj-models"
contract in the docs is false. `aj-conf` itself depends only on leaf
crates, so this is not a cycle — but it is a layer the runtime shouldn't
reach into.
**Suggested action:** Decide with the user whether the graph in
`CLAUDE.md` is wrong (i.e. `aj-conf` is intended to sit below `aj-agent`
and the doc should list the edge) or whether the runtime should be
decoupled from `aj-conf`. If the latter: move `AgentEnv` and a
runtime-native thinking-level enum into `aj-agent` (or `aj-models`) and
have the binary do the `ConfigThinkingLevel` translation, so `aj-agent`
takes a plain environment struct it owns. Either way the doc and the
manifest must agree.
**Effort:** M

### [Major][Errors] The runtime trusts the provider's `Done` terminal blindly — a truncated turn (M3/M4) lands on the transcript as a complete success with no guard — `src/aj-agent/src/lib.rs:822-832,883-893`
**What:** In `execute_turn` the poll loop captures the terminal frame: a
`Done { message }` sets `final_message = Some(message)` /
`final_was_error = false` (`lib.rs:823-826`); the success path then appends
that message to the transcript and proceeds with its `stop_reason`
verbatim. There is no agent-side re-classification, content-vs-stop-reason
sanity check, or retry on a successful-but-suspicious `Done`. The only
self-defense is the `final_message == None` branch (`lib.rs:885-892`),
which fires *only when the stream's channel closed without any Done/Error
event ever being captured* — it calls `response_stream.result()`, which
synthesizes a `Transient` error (`streaming.rs:378-390`). But the M3/M4
finding is precisely that the providers finalize a stream-end-without-
terminal as a real `AssistantMessageEvent::Done(Stop)` *before* the channel
closes; that Done is captured here, so the agent takes the success path and
the `None` fallback is never reached for the truncation case.
**Why it matters:** This is the consuming end of the workspace's
confirmed four-provider truncation-looks-like-success bug. A transport
hiccup that drops the connection mid-turn (after some deltas, before the
terminal frame) becomes a `Done(Stop)` at the provider, is trusted here,
and is appended to the transcript as a finished assistant turn — so the
agent loop neither retries (it only retries `Overloaded`, see `lib.rs:943`)
nor surfaces an error; the user silently gets a truncated answer and the
under-counted usage is accumulated as final. The runtime is the natural
last line of defense (it owns the retry budget and the transcript), and it
has none for this case.
**Suggested action:** Land the fix at the provider layer (M3/M4: classify
stream-end-without-terminal as a retryable `Error`) so the agent sees an
`Error` it already routes through its recoverable/overloaded path — and/or
add an agent-side guard that treats a `Done` whose accumulated content is
empty/structurally incomplete as recoverable. Decide the single
cross-provider policy with the user alongside the M3/M4 findings. At
minimum, document at `lib.rs:822` that the runtime trusts the provider's
terminal classification and therefore inherits its truncation semantics.
**Effort:** M

### [Major][Simplicity] Three event variants are declared but never emitted — `TurnEnd`, `QueueUpdate`, `ToolExecutionUpdate` — `src/aj-agent/src/events.rs:91,152,232`
**What:** `rg "emit\(AgentEvent::(TurnEnd|QueueUpdate|ToolExecutionUpdate)"`
across the whole workspace returns nothing — no code path constructs or
emits any of these. `TurnEnd` (`events.rs:91`, carries `message` +
`tool_results`) is acknowledged as deferred by the `execute_turn` comment
"The matching `TurnEnd` event … lands in §2.4 once `aj-agent` migrates to
the unified message types" (`lib.rs:742-745`) — but the agent *already*
carries unified messages (the projection module's own doc says "After the
wire-format flip the agent's transcript already carries unified
messages"), so the stated blocker is gone and `TurnEnd` is now just unused.
`ToolExecutionUpdate` (`events.rs:152`) has a permanent no-op producer
(`emit_update` at `lib.rs:1729`). `QueueUpdate` (`events.rs:232`) has no
producer anywhere; its `steering`/`follow_up` queues are a binary-side
concept the runtime doesn't model. All three are matched in consumers
(`event_pump.rs`, the test `label` fn) only to be ignored or stringified.
**Why it matters:** Dead surface in the *defining* taxonomy of the
architecture. The events file is the contract every frontend codes
against; three variants that never fire mislead a reader into thinking the
agent emits per-turn finalization (`TurnEnd`), tool progress
(`ToolExecutionUpdate`), and queue snapshots (`QueueUpdate`) when it does
none of these. The `TurnUsage` variant (`events.rs:223`) even exists as a
"bridging variant" to do part of what `TurnEnd` was meant to, so the two
overlap. Consumers carry no-op match arms for events that can't occur.
**Suggested action:** For each: either wire the producer (emit `TurnEnd`
at the end of `execute_turn` now that unified messages are available, and
collapse/justify the `TurnUsage` overlap), or delete the variant and its
no-op consumer arms. `ToolExecutionUpdate` should stay only if the
`emit_update` async-bus story (lib.rs:1729 comment) is actually scheduled;
otherwise drop both. Decide with the user which are roadmap vs. dead.
**Effort:** M

### [Major][Boundaries] `tools::Tool` ↔ `ToolDefinition` round-trip lives on this exact boundary — the runtime holds `Vec<Tool>` only to remap it to `ToolDefinition` at inference time — `src/aj-agent/src/lib.rs:64,167-175,1348-1356`
**What:** Confirming the M1 Major finding at the consuming end. `Agent`
stores `tools: Vec<Tool>` (`lib.rs:64`), built in `with_provider` from the
erased tool definitions by copying `name`/`description`/`input_schema`
and hardcoding `r#type: None` (`lib.rs:167-175`). Then
`run_inference_streaming` maps that `Vec<Tool>` field-by-field onto
`Vec<UnifiedToolDefinition>` to populate the `Context` (`lib.rs:1348-1356`,
`input_schema` → `parameters`). So the data path is `ErasedToolDefinition
→ Tool → ToolDefinition`, and the intermediate `aj_models::tools::Tool`
holds nothing the final shape uses (`r#type` is never read). The agent
already has the `ErasedToolDefinition`s in `tool_definitions`
(`lib.rs:63`); it could build `ToolDefinition` directly.
**Why it matters:** The runtime carries a redundant `Vec<Tool>` field and
performs a per-inference remap purely to bridge two `aj-models` types for
one concept. It's the cross-boundary cost the M1 finding predicted, now
observed: a reader has to follow three representations of "the tool
definitions" through the agent, and the `input_schema`/`parameters`
rename invites confusion at the seam.
**Suggested action:** Drop the `tools: Vec<Tool>` field; build
`Vec<UnifiedToolDefinition>` once in `with_provider` (or lazily in
`run_inference_streaming`) straight from `tool_definitions.values()`.
Coordinate with the M1 decision on whether `tools::Tool` survives at all.
**Effort:** S

### [Minor][Errors] `Agent` is a library type but its public turn API and internals lean on `anyhow` — `src/aj-agent/src/lib.rs:556,634,690,993,1454,1796,1801`
**What:** `TurnError` is a proper `thiserror` enum (`lib.rs:1790`), but two
of its three variants wrap `anyhow::Error` (`Recoverable(anyhow::Error)`,
`Fatal(anyhow::Error)`), `run_single_turn` returns
`Result<String, anyhow::Error>` (`lib.rs:634`), `execute_tool` returns
`anyhow::Error` (`lib.rs:1454`, with `anyhow!("tool not found!")`), and
recoverable failures are built with `anyhow!(detail)` (`lib.rs:993`). The
`From<anyhow::Error> for TurnError` impl (`lib.rs:1813`) routes any stray
`anyhow` into `Fatal`. This is the same "anyhow in a lib crate" theme M1
flagged for `refresh.rs`, here in the runtime's primary API.
**Why it matters:** A caller of `prompt`/`continue_run` can distinguish
the three `TurnError` arms but cannot programmatically inspect *why* a
`Recoverable`/`Fatal` happened (auth vs. transport vs. tool-not-found vs.
listener-write-failure) — only render the string. `run_single_turn`'s
`anyhow::Error` return type leaks an untyped error across the public
sub-agent seam. The runtime is a library crate per the graph, so this
widens the `anyhow` dependency where a typed error would serve callers
better.
**Suggested action:** Consider giving the inner errors more structure
(e.g. carry the originating `AssistantError`/`ErrorCategory` on
`Recoverable` so the binary can branch on rate-limit vs. transient), and
return `TurnError` rather than bare `anyhow::Error` from `run_single_turn`
/ `execute_tool`. Fold into the workspace-wide anyhow-in-lib-crates
decision from synthesis rather than fixing piecemeal.
**Effort:** M

### [Minor][Contracts] `expect`s on `assembled_system_prompt` are reachable from the public API without a type-enforced precondition — `src/aj-agent/src/lib.rs:1330,1477`
**What:** `run_inference_streaming` does
`self.assembled_system_prompt.clone().expect("system prompt must be
resolved before inference")` (`lib.rs:1330`) and `execute_tool` does the
same for the sub-agent path (`lib.rs:1477`). The precondition — "call
`set_assembled_system_prompt` before any turn" — is documented on the
setter (`lib.rs:396`) and the field (`lib.rs:58`), but it's enforced only
by convention: nothing in the type system stops a caller from invoking
`prompt`/`continue_run`/`run_single_turn` on a freshly constructed agent
whose prompt was never set, which panics rather than returning
`TurnError`. The `current_turn`/`messages` accessors and the constructor
don't require it.
**Why it matters:** A reachable panic on the public turn path keyed off an
invariant the API doesn't make impossible to violate. A host that wires
the agent slightly differently (or a future caller) gets a panic instead
of a `TurnError::Fatal`. The two other `expect`s in the file
(`retry_strategy` "known to be some" at `lib.rs:950`, mutex poisoning in
`bus.rs`) are locally justified; these two cross a public boundary.
**Suggested action:** Either make the prompt non-optional by requiring it
at construction (move it into `with_provider`, since the binary already
calls the setter exactly once before the first turn), or convert the
`expect`s into an early `return Err(TurnError::Fatal(...))` so the
mis-sequencing surfaces as a typed error. Prefer the former — it makes the
invariant a type guarantee.
**Effort:** S

### [Minor][Simplicity] `AgentEnd.messages` and the empty-snapshot emit are vestigial — the event field is always shipped empty — `src/aj-agent/src/lib.rs:588-591,648-651`, `events.rs:83-86`
**What:** `AgentEnd { messages: Vec<AgentMessage> }` (`events.rs:83`) is
documented as carrying "the full transcript for listeners that want a
final snapshot," but both emit sites pass `messages: Vec::new()`
(`lib.rs:591`, `lib.rs:651`) with the comment "until §2.4 migrates the
agent … we ship an empty snapshot" (`lib.rs:572-577`). As with `TurnEnd`,
the cited migration is already done (the transcript is unified messages),
so the field is permanently empty and the documented contract
("carries the full transcript") is false today.
**Why it matters:** A serialized event field (`AgentEnd` round-trips
through `aj --format json`) that always carries `[]` while its doc
promises a snapshot — a listener coding to the doc gets nothing. Either
the field is useful and should be populated (`self.transcript.clone()` is
right there) or it's noise.
**Suggested action:** Populate `messages` with `self.transcript.clone()`
at both emit sites (the agent owns the transcript), or drop the field and
its doc. Resolve alongside the `TurnEnd` finding since both are gated on
the same already-completed migration.
**Effort:** S

### [Minor][Dependencies] Manifest hardcodes `edition = "2021"` and `version = "0.1.0"` instead of inheriting the 2024 workspace — `src/aj-agent/Cargo.toml:4,3`
**What:** `aj-agent/Cargo.toml` sets `edition = "2021"` and
`version = "0.1.0"` literally, whereas sibling crates use
`edition.workspace = true` / `version.workspace = true` (e.g.
`aj-conf/Cargo.toml:3-4`, `aj-models/Cargo.toml:4`) and the workspace
declares `edition = "2024"` (`Cargo.toml:17`). `CLAUDE.md` states the code
style is "Rust edition 2024." So this crate compiles under an older
edition than the rest of the workspace and won't follow the workspace
version if it bumps.
**Why it matters:** Dependency/style hygiene: the crate silently diverges
from the workspace edition (different default lints, idioms, and
prelude), and a future workspace version bump won't propagate. It's the
kind of drift the rubric's edition check flags.
**Suggested action:** Switch to `edition.workspace = true` and
`version.workspace = true`, then `cargo check` to confirm the crate
builds clean under edition 2024 (it almost certainly does — the code reads
as 2021-compatible 2024).
**Effort:** S

### [Nit][Comments] Several comments narrate `docs/aj-next-plan.md` "§N lands later / until §N / for now" chronology — `src/aj-agent/src/lib.rs:100-108,572-577,742-745,1729-1741`, `events.rs:67`
**What:** Recurring chronology framing the rubric calls out: the
`cancellation` field doc says "Today the agent never fires it:
cancellation propagation lands in §1.8" (`lib.rs:101`) — but the agent
*does* fire and thread cancellation throughout `execute_turn`, so this is
stale and contradicts the code below it; `AgentEnd` emits "until §2.4
migrates the agent" (`lib.rs:572`); `TurnEnd` "lands in §2.4 once aj-agent
migrates" (`lib.rs:742`); `emit_update` "No-op for now … lands when the
TUI needs progress streaming" (`lib.rs:1729`). These describe past/future
plan states rather than the current contract.
**Why it matters:** Comments should stand on their own from the current
code; the `cancellation`-field comment is actively wrong (cancellation is
implemented), which is worse than fluff. The `docs/aj-next-plan.md` §
references couple the code's comments to an external roadmap doc.
**Suggested action:** Restate each as the steady-state contract. For the
`cancellation` field: describe what it *does* (the token threaded into
the provider and `select!`-ed at three checkpoints). Drop "for now" / "§N
lands later" phrasing; keep only the why where it records a real
constraint (e.g. the `emit_update` sync-vs-async-bus rationale is worth
keeping, minus the "lands when" clause).
**Effort:** S

### [Nit][Testing] No test exercises the streaming-`select!` cancel arm in isolation or a multi-tool-batch partial-cancel — `src/aj-agent/src/lib.rs:806-812,1162-1186`
**What:** The cancellation suite is good but has two gaps against the
documented §1.8 invariant. `cancel_mid_stream_pushes_aborted_partial`
(`lib.rs:2806`) uses a 60s-gated provider Done and a 50ms timer to *race*
the agent-side `select!` cancel arm (`lib.rs:809`) — it's the only test
that can hit that arm, and it does so via wall-clock timing, so it's both
the sole coverage and a (mild) flakiness risk if a loaded runner doesn't
fire the cancel before the 60s Done is polled. Separately, the
"synthesize `tool_result` for every still-pending tool call" path
(`lib.rs:1171-1184`) — the part of the invariant that keeps a multi-tool
batch internally consistent on cancel — is never exercised: no test
cancels a turn with more than one tool call queued.
**Why it matters:** The transcript-consistency invariant the module doc
makes the centerpiece of cancellation ("no dangling `tool_use` without a
matching `tool_result`") is only tested for the zero-pending-tools case.
The most failure-prone branch (mid-batch cancel with remaining calls) has
no boundary test.
**Suggested action:** Add a test that scripts a single assistant turn with
two tool calls and a `before_tool_call` hook (or a deterministic signal)
that cancels during the first tool, asserting the transcript ends with
matching `is_error` tool-results for *both* calls and `TurnError::Aborted`.
Consider a deterministic cancel trigger (cancel from inside the first
tool's `execute` via the shared token) to remove the wall-clock race from
the streaming-arm test too.
**Effort:** M

### [Nit][Style] `determine_thinking` trigger phrases sit on a non-obvious "most specific first" ordering enforced by code order, not documented at the match — `src/aj-agent/src/lib.rs:1427-1438`
**What:** The trigger ladder checks `"think maximum"` → `"think hardest"`
→ `"think harder"` → `"think hard"` → `"think"` (`lib.rs:1428-1437`). The
doc comment lists *different* phrases ("think harder → 32,000 tokens",
etc., `lib.rs:1387-1390`) than the code actually matches, and the
load-bearing invariant — that the checks must stay ordered most-specific-
first because every longer phrase contains a shorter one as a substring
(`"think hard"` contains `"think"`) — is only implied by ordering.
**Why it matters:** The doc/code mismatch (the doc's token numbers and
phrase set don't match the `ThinkingConfig` rungs the code returns) is a
small contract drift, and the substring-ordering dependency is an
invariant a reorder would silently break.
**Suggested action:** Sync the doc comment to the actual phrases/rungs and
add one line noting the checks must stay most-specific-first because
shorter triggers are substrings of longer ones.
**Effort:** S

## What's good

- **The turn loop is a clean, single-owner state machine
  (`lib.rs:738`).** `execute_turn` is the one place that drives inference
  → terminal-frame capture → retry/abort routing → tool batch → loop. The
  three terminal outcomes (saw `Done`/`Error`, channel-closed fallback,
  agent-side cancel) are enumerated explicitly with a comment block
  (`lib.rs:860-869`), and the success/error/abort branches each push the
  finalized message and emit a `MessageEnd` so listeners see a uniform
  shape regardless of how the turn ended.
- **Persistence really is "just a subscriber."** `Agent::prompt` takes no
  log, the agent holds no `ConversationLog`, and the only write path is
  `bus.emit(MessageEnd)` which the binary's `aj-session` listener
  consumes. The bus's inline-await contract (`bus.rs:1-13`) gives the
  "log never more than one event behind reality" durability guarantee for
  free, and a listener `Err` becomes `TurnError::Fatal` so a disk failure
  aborts rather than silently dropping writes. The architecture's central
  claim holds.
- **The event bus is small and correct (`bus.rs`).** Subscribe/emit/drop
  are textbook: emit snapshots the listener `Arc`s under a short lock then
  awaits outside it (no lock held across `.await`), `SubscriptionHandle`
  holds a `Weak` so a handle outliving the bus is an inert drop (tested at
  `bus.rs:298`), registration-order dispatch and error-short-circuit are
  both tested. `subscribe_channel` (`lib.rs:301`) layers the non-blocking
  mpsc-forwarder sugar on top without a special-case API — exactly the
  composition the module doc promises.
- **Cancellation design (`lib.rs:716-737, 803-812, 1107-1132`).** Three
  documented checkpoints (pre-iteration atomic check, streaming `select!`,
  per-tool `select!`), `biased` so the cancel arm wins ties, the retry
  sleep is itself `select!`-ed against cancel, and the transcript is left
  consistent (aborted partial pushed, pending tool-calls get synthetic
  `is_error` results). Both halves of the cancel race
  (agent-side `select!` and provider-side `Error{Aborted}`) funnel to one
  `TurnError::Aborted`, and that convergence is tested
  (`prompt_returns_aborted_on_provider_side_cancel`,
  `prompt_returns_aborted_when_token_fired_before_call`).
- **Hooks are a well-scoped seam (`hooks.rs`).** Three single-slot
  `Option<Arc<dyn Fn…>>` hooks (before/after tool, should-stop-after-turn),
  each awaited inline like a listener, with a clear contract and the
  explicit "no registry, no priority — compose into one closure" stance.
  The before-hook's `Proceed{args}` / `ShortCircuit{outcome}` enum is a
  clean two-way decision, and the after-hook's mutate-in-place is the
  right shape for redaction/truncation. Not a god-hook.
- **`AgentId` routing and the event taxonomy's serialization contract.**
  Every variant carries an `AgentId`, `agent_id()` collapses them with the
  documented "sub-events report the parent" rule (tested at
  `events.rs:273`), and the internally-tagged snake_case JSON shape is
  pinned by a round-trip test (`events.rs:306`) so the `aj --format json`
  consumer contract can't silently drift. The `Arc<[UserContent]>` custom
  serializer keeps image-bearing tool results cheap to fan out while
  serializing as a plain sequence.
- **`event_protocol_tests` pins the emitted sequence (`lib.rs:1871`).**
  The `EventLabel` snapshot approach locks the exact ordered bus protocol
  for tool-call turns, plain-text turns, `prompt` vs `continue_run`, and
  the abort paths — running the agent in isolation against a strict-mode
  `ScriptedProvider` (panics on an unexpected extra inference). This is
  high-quality boundary testing of the runtime's only output channel.
- **`finalize_tool_result` shares the success and cancel paths
  (`lib.rs:1221`).** One projection of `ToolOutcome → Message::ToolResult`
  + the `MessageStart`/`MessageEnd`/`ToolExecutionEnd` bracket, used by
  both the normal and cancelled-tool paths, so the persisted shape is
  identical regardless of why the outcome was produced. The structured
  `details` riding both on the event (live render) and serialized onto the
  wire message (resume) is tested end-to-end (`lib.rs:2398`).

## Boundary & architecture notes

The intended graph in `CLAUDE.md` is `aj-models ← aj-agent ← aj-tools`
with `aj-session` and `aj` above, and `aj-agent` described as depending
"only on aj-models." The real dependency set is `aj-models` **and
`aj-conf`** (`Cargo.toml:7-9`; the Critical finding). `aj-conf` is a leaf
config crate so there's no cycle, but the edge is undocumented and pulls
config-file machinery into the runtime. The synthesis step should
reconcile the graph with reality: either add `aj-conf` below `aj-agent` in
`CLAUDE.md`, or decouple the runtime from it.

The runtime's design does honor the rest of the architecture: no UI
dependency, no `aj-session`/persistence dependency (persistence is a bus
subscriber registered by the binary), and the `Provider` trait is the only
inference seam. The `SessionContextWrapper` (`lib.rs:1559`) is a tidy
partial-borrow helper, not a leak. Sub-agents share the parent's bus,
provider, model_info, and a `child_token` cancellation — a coherent
hierarchy.

Public-surface note: the runtime's public API is broad but mostly
justified (it's the binary's whole control surface). The `#[cfg(test)]`
`scan_dangling_tool_uses` (`lib.rs:1757`) is correctly test-gated.
`SessionState` is `pub` with several `pub` accessors; some
(`record_sub_agent_usage` is private, `next_sub_agent_id` is private) are
correctly scoped, but `SessionState::new` and the field `sub_agent_usage`
(`lib.rs:1503`, accessed directly at `lib.rs:337`) are `pub` — worth a
glance in AG2/synthesis for whether `SessionState` needs to be public at
all or could be `pub(crate)`.

Cross-boundary type duplication: the M1 `tools::Tool` vs `ToolDefinition`
round-trip is confirmed here (Major finding) — this is the consumer the M1
audit predicted.

## Test assessment

Tests are in-module under `#[cfg(test)]` per convention and overwhelmingly
exercise the boundary, not internals:

- **lib.rs `event_protocol_tests`** is the strongest unit: it pins the
  exact ordered `AgentEvent` sequence the agent emits for representative
  turns (tool-call, plain-text, `prompt`, `continue_run`), the abort paths
  (provider-side, pre-cancel, mid-stream), all three hooks (before-mutate,
  before-short-circuit, after-rewrite, should-stop), and the structured-
  details persistence contract — all through the public entry points,
  against a strict-mode scripted provider. Excellent boundary coverage.
- **bus.rs** covers registration-order dispatch, drop-unsubscribes,
  error-propagation-short-circuits, and handle-outliving-bus. The full bus
  contract is tested.
- **events.rs** pins `agent_id()` parent-routing and the JSON wire shape.
- **projection.rs** round-trips a mixed transcript through
  `transcript_to_messages`.
- **hooks.rs** compile/shape-checks each hook type.

Gaps (see findings):
- The agent-side streaming `select!` cancel arm is covered only by a
  single wall-clock-raced test, and the multi-tool partial-cancel branch
  (synthesize results for all remaining calls) is untested (Nit).
- **No test covers the truncation-as-`Done` path** — unsurprising, since
  the runtime trusts the provider's terminal and the scripted provider
  always emits a well-formed terminal. A test that scripts a `Done` with
  empty/partial content would document the runtime's current
  (mis)handling and pin whatever guard the M3/M4 fix adds.
- The `final_message == None` channel-closed fallback (`lib.rs:885`) is
  never exercised: no test drives a stream that closes without any
  terminal event, so the `result()`-synthesizes-`Transient` recovery is
  unverified at the agent layer.
- No test covers the `Overloaded` retry/backoff loop (`lib.rs:946-985`) or
  retry-exhaustion → `Recoverable`. The retry path — the one place the
  runtime makes a transient-error policy decision — has zero coverage.

Fixtures are inline and readable; the `ScriptedProvider` + `ProviderScript`
harness is well-used. The only flakiness risk is the 50ms/60s timing race
in `cancel_mid_stream_pushes_aborted_partial`.

## Cross-cutting themes to bubble up

- **Truncation-looks-like-success (CONFIRMED at the consumer).** The
  four-provider M3/M4 bug surfaces straight through the runtime: a `Done`
  is trusted with no guard, so a truncated turn lands as a complete
  success and isn't retried. The runtime's own channel-closed fallback
  doesn't help because the providers finalize before the channel closes.
  Synthesis should drive one policy and decide whether the guard lives in
  the provider, the runtime, or both.
- **anyhow in lib crates (CONFIRMED, new locus).** `aj-agent` is a library
  crate per the graph, yet `TurnError` wraps `anyhow::Error`,
  `run_single_turn`/`execute_tool` return `anyhow::Error`, and recoverable
  errors are `anyhow!`-built. Same theme as M1's `refresh.rs` and the
  SDKs. One workspace decision.
- **Stated dependency graph vs. reality (NEW, Critical).** `aj-agent`
  depends on `aj-conf`, an edge absent from `CLAUDE.md`'s graph. Synthesis
  must verify *every* crate's real edges against the doc — this is the
  first confirmed mismatch and likely not the last.
- **Dead/aspirational declared surface (NEW).** Three event variants
  (`TurnEnd`, `QueueUpdate`, `ToolExecutionUpdate`) and the `AgentEnd.messages`
  field are declared and consumer-matched but never produced. Analogous
  to M1/M4's test-only-`pub` and dead-field themes, but here in the
  architecture's defining event taxonomy. Synthesis should sweep for
  "declared in the contract, never emitted/used."
- **Chronology / roadmap-doc references in comments (CONFIRMED,
  recurring).** "§N lands later / for now / until §N migrates" framing
  recurs (here coupled to `docs/aj-next-plan.md`), and one instance (the
  `cancellation` field) is actively wrong. Same comment-hygiene theme as
  M1's `provider.rs`. Synthesis should note the consistent reliance on
  external-doc § references in comments.
- **Manifest inheritance drift (NEW, minor).** `aj-agent` hardcodes
  `edition = "2021"` / `version = "0.1.0"` instead of inheriting the
  workspace's 2024/`.workspace = true`. Worth a sweep of all manifests in
  synthesis for crates not inheriting workspace package fields.
