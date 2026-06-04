# Audit findings — aj-agent-contracts

- **Step:** AG2
- **Date:** 2026-06-02
- **Audited commit:** f5950da
- **Scope:** `src/aj-agent/src/tool.rs`, `src/aj-agent/src/message.rs`,
  `src/aj-agent/src/types.rs` (incl. the in-module `#[cfg(test)]` suites in
  `tool.rs` and `message.rs`).

## Summary

These are the contract types the rest of the workspace codes against, and
they are mostly well-designed. The tool trait is a clean, minimal seam:
typed `Input` with `JsonSchema`-derived schemas, a blanket
`From<T> for ErasedToolDefinition` that erases the type for heterogeneous
storage, and an `async fn execute` expressed via RPITIT (no `async_trait`
in the public surface). `ToolDetails` is a deliberately neutral,
closed-by-rendering-shape data enum that round-trips through JSONL and does
not import any UI crate — a genuinely good contract. `AgentMessage` is a
disciplined forward-compat seam over wire `Message`: `#[serde(transparent)]`
+ `untagged` keep the on-disk shape flat and lossless, the backward-compat
behavior is explicitly tested, and there is no duplication of wire types.
None of the three files pull in UI or persistence dependencies; their only
external crate edge is `aj-models` (plus `schemars`/`serde`), which is
correct for the contract layer.

Two things stand out against the rubric. First, the trait's `execute`
returns `anyhow::Result` and `ToolContext::spawn_agent` does too — the
"anyhow in lib crates" theme reaches into the public tool-author contract,
so every tool implementor depends on `anyhow` and the agent can only render
a tool failure as a string. Second, `ToolContext::emit_update` is
documented as emitting a `ToolExecutionUpdate` bus event, but the runtime's
only impl is a permanent no-op (`lib.rs:1729`); this is the
contract-layer locus of the dead `ToolExecutionUpdate` variant AG1 flagged
— tool authors (e.g. `bash`) self-throttle snapshots that go nowhere. The
AG1/M1 `Tool` ↔ `ToolDefinition` duplication does **not** originate here:
the contract layer cleanly produces `ErasedToolDefinition`; the redundant
`tools::Tool` hop is introduced by the runtime (`lib.rs`), not by these
types.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 4 | 2 |

## Findings

### [Major][Contracts] `ToolContext::emit_update` documents a bus event it never produces — tools self-throttle snapshots into a no-op — `src/aj-agent/src/tool.rs:439-443`
**What:** The trait method's doc says: "Emit a partial [`ToolDetails`]
snapshot through the bus as a
[`crate::events::AgentEvent::ToolExecutionUpdate`]. Tools that produce many
updates self-throttle (~10/s); the agent does not debounce." But the only
production `ToolContext` impl (`Agent`'s wrapper at `lib.rs:1729`) is
`fn emit_update(&mut self, _partial: ToolDetails) {}` — a documented no-op.
`ToolExecutionUpdate` is never emitted anywhere in the workspace (confirmed
by AG1; `rg "emit\(AgentEvent::ToolExecutionUpdate"` is empty). Tool authors
take the doc at face value: `bash` builds rolling snapshots and rate-limits
them to ~10/s (`aj-tools/src/tools/bash.rs:97,220`) specifically to feed
this method, and `aj`'s `event_pump`/`bash_execution`/`tool_execution`
components carry match arms and a 100ms debounce for an event that can't
arrive.
**Why it matters:** This is a contract that lies at the *defining* trait of
the tool boundary. A tool author reads "emit a snapshot through the bus,"
does the throttling work, and gets nothing — the progress-streaming feature
appears wired end-to-end (tool side and TUI side both implement it) but the
runtime drops it on the floor. It's wasted work in every long-running tool
and dead match arms in the frontend, all anchored to a false method
contract. AG1 flagged the dead `ToolExecutionUpdate` *variant*; this is its
root: the method that is supposed to produce it.
**Suggested action:** Decide with the user whether progress streaming is
roadmap or dead. If roadmap, wire the no-op to a sync bus emit (the
`lib.rs:1730-1741` comment already sketches the `try_emit_sync` design) so
the documented contract becomes true. If not, change the method doc to state
it is currently a no-op (and ideally remove the variant + frontend arms per
AG1). Either way the trait doc must match runtime behavior.
**Effort:** M

### [Minor][Errors] The tool trait's public contract is built on `anyhow` — every tool author depends on it and failures are only renderable as strings — `src/aj-agent/src/tool.rs:344-348,434-437`
**What:** `ToolDefinition::execute` returns
`impl Future<Output = anyhow::Result<ToolOutcome>>` (`tool.rs:348`),
`ErasedToolFn` wraps the same `anyhow::Result` (`tool.rs:362`), and
`ToolContext::spawn_agent` returns `anyhow::Result<SpawnedAgent>`
(`tool.rs:437`). So `anyhow` is part of the public seam every tool
implementor must adopt. The trait doc (`tool.rs:340-343`) tells authors to
prefer `is_error: true` outcomes for recoverable failures and reserve `Err`
for turn-aborting cases — a sound convention — but an `Err` carries an
untyped `anyhow::Error` the agent can only stringify.
**Why it matters:** This is the contract-layer instance of the workspace
"anyhow in lib crates" theme (AG1 flagged it in the runtime's `TurnError`;
M1 in `refresh.rs`). Here it propagates outward to *every downstream tool
crate*: `aj-tools` and any third-party tool must take an `anyhow`
dependency to satisfy the trait, and the agent can't programmatically
distinguish a tool's failure modes. It also couples the abstraction to a
specific error library at its most-depended-on interface.
**Suggested action:** Fold into the workspace-wide anyhow decision rather
than fixing piecemeal. Options: introduce a `thiserror` `ToolError` the
trait returns, or keep `anyhow` but document at `tool.rs:344` that the trait
intentionally accepts an opaque error and the agent only renders it (so the
`anyhow` choice is a stated contract, not an accident).
**Effort:** M

### [Minor][Contracts] `SpawnedAgent.agent_id` / `ToolDetails::SubAgentReport.agent_id` are `usize`, decoupled from the `AgentId::Sub(_)` newtype they must match — `src/aj-agent/src/tool.rs:135-142,297-302`
**What:** `SpawnedAgent.agent_id: usize` (`tool.rs:300`) and
`ToolDetails::SubAgentReport { agent_id: usize, .. }` (`tool.rs:137`) are
raw `usize`s whose doc says they "match the `Sub(_)` index surfaced on the
bus through `AgentEvent::SubAgentStart`/`SubAgentEnd`" (`tool.rs:294-296`).
The relationship between this `usize` and `events::AgentId::Sub(n)` is
enforced only by the prose comment and by the `agent` builtin copying one
into the other; nothing in the types ties them together, so a drift (e.g.
off-by-one between the allocated id and the rendered id) wouldn't be caught
by the compiler.
**Why it matters:** Contract-by-convention across a boundary the audit
specifically cares about — these are the shared types `aj-tools` consumes
and `aj-session`/TUI render. A reader has to cross-reference three doc
comments to learn that the `usize` here is "the same number as
`AgentId::Sub`." It's a missing-type-relationship smell rather than a bug.
**Suggested action:** Consider carrying `AgentId` (or a dedicated
`SubAgentId(usize)` newtype) on `SpawnedAgent` and `SubAgentReport` so the
"these are the same id" invariant is a type fact, not a comment. Low
priority — flag for the synthesis pass on id typing across the event/tool
boundary.
**Effort:** S

### [Minor][Comments] `tool.rs` / `message.rs` module docs reference `docs/aj-next-plan.md §N` and narrate a pending migration — `src/aj-agent/src/tool.rs:9`, `src/aj-agent/src/message.rs:10-16`
**What:** Same chronology/roadmap-doc theme AG1 flagged in the runtime.
`tool.rs:9` ends its module doc with "See `docs/aj-next-plan.md` §1.2 and
§1.3." `message.rs:10-16` says "Today there is exactly one variant … New
variants land alongside their use sites … See `docs/aj-next-plan.md` §1.4
and §2.0(b)." The `message.rs` module doc (`message.rs:6-8`) also frames the
wrapper as something the transcript "can later be extended" — describing a
future state. The test comment at `tool.rs:491` says "so the upcoming
`aj-session` migration walker can rely on a stable shape," but per AG1 that
migration is already complete (the transcript already carries unified
messages).
**Why it matters:** Comments should stand on their own from the current
code; coupling them to external plan-doc section numbers and to an
"upcoming" migration that has landed makes them go stale. The
forward-compat *intent* of `AgentMessage` is worth keeping (it explains the
otherwise-puzzling single-variant enum), but it should read as a stable
design rationale, not a roadmap.
**Suggested action:** Drop the `§N` references; restate the
`AgentMessage`/`AgentMessageKind` docs as steady-state design rationale
("single variant today; the wrapper is the forward-compat seam for
agent-only entries"). Update the `tool.rs:491` test comment to drop
"upcoming." Resolve alongside AG1's chronology finding.
**Effort:** S

### [Minor][Testing] No test pins the schema contract `derive_schema` promises (stripped `title`, always-present `properties`/`required`) or the legacy `[Output truncated]` fallback — `src/aj-agent/src/tool.rs:460-481,104-103`
**What:** `derive_schema` makes three load-bearing guarantees the
provider adapters depend on: it removes `title`, and it injects empty
`properties`/`required` when absent so the schema validates as an
Anthropic/OpenAI function-parameter object (`tool.rs:471-478`). None of
these is tested — the `#[cfg(test)]` module covers `ToolDetails`
round-tripping and the `ExecutionMode` default, but not the schema
helper, even though it sits on the wire boundary. Separately, the
`ToolDetails::Bash` doc promises that "absent fields on older persisted
sessions fall back to the legacy `[Output truncated]` marker"
(`tool.rs:101-103`); the round-trip test only exercises the all-`None` /
all-present case, not deserialization of a legacy `Bash` payload missing
the `stdout_truncation`/`stderr_truncation`/`truncated` fields.
**Why it matters:** `derive_schema` is the function that turns every
tool's `Input` into the JSON the model sees; a regression (e.g. a
`schemars` upgrade reintroducing `title`, or an input type with no fields
producing a schema without `properties`) would silently break tool calls
and isn't guarded. The legacy-Bash deserialization is a stated
backward-compat contract for persisted sessions with no test, where the
analogous legacy path for `AgentMessage` *is* tested
(`message.rs:129`). Happy-path-only coverage on the two parts of this file
that face external schemas/old data.
**Suggested action:** Add a test that derives a schema for a struct input
and a unit (fieldless) input, asserting `title` is absent and
`properties`/`required` exist in both. Add a `ToolDetails::Bash`
deserialization test from a JSON object omitting the
truncation/`truncated` fields, asserting it defaults cleanly.
**Effort:** S

### [Nit][Contracts] `derive_schema`'s `expect("invalid schema object")` is reachable from any tool author's `Input` — justify or document — `src/aj-agent/src/tool.rs:480`
**What:** `derive_schema` ends with
`serde_json::to_value(&schema).expect("invalid schema object")`. The schema
value originates from `schemars` and is realistically always serializable,
so the `expect` is practically unreachable — but it sits on the default
`input_schema()` path that runs for every tool, keyed off an author-supplied
`JsonSchema` type. There's no comment noting why the panic is acceptable.
**Why it matters:** Minor reachable-`unwrap`/`expect`-in-lib-code item per
the rubric: a panic on a tool author's input type, with the justification
("a `schemars`-produced schema always serializes to a `Value`") left
implicit.
**Suggested action:** Add a one-line nota bene that the value comes from
`schemars` and is always serializable, so the `expect` is documented as a
true invariant rather than an unchecked panic.
**Effort:** S

### [Nit][Comments] `ToolDetails` Bash doc has a stray duplicated-clause sentence about truncation — `src/aj-agent/src/tool.rs:94-103`
**What:** The `Bash` variant's leading doc paragraph already explains the
`stdout_truncation`/`stderr_truncation` payloads and the legacy fallback,
and then each field repeats the same contract in its own doc
(`tool.rs:121-131`). The variant-level paragraph (`tool.rs:94-103`) and the
field-level docs overlap substantially, restating "renderers use this to
compose informative markers" twice.
**Why it matters:** Mild fluff/duplication in an otherwise excellent set of
contract docs — the kind of repetition the guidance discourages. Not wrong,
just redundant.
**Suggested action:** Trim the variant-level paragraph to the cross-field
invariant (the tail/cap/spill behavior) and let the per-field docs carry the
per-field contract.
**Effort:** S

## What's good

- **The tool trait is a clean, minimal seam (`tool.rs:314-349`).** Five
  methods, three with sensible defaults (`input_schema`, `execution_mode`
  via `Default`), a strongly-typed associated `Input`, and an `async
  execute` expressed as RPITIT (`impl Future + Send`) so the *public* trait
  needs no `async_trait` (it's only a dev-dependency). Tool authors don't
  touch runtime internals: they get a `&mut dyn ToolContext` and return a
  `ToolOutcome`. The `From<T> for ErasedToolDefinition` blanket impl
  (`tool.rs:381-405`) is the right erasure boundary — typed authoring,
  heterogeneous storage — and parses raw JSON into `Self::Input` inside the
  erased closure so deserialization failures surface as the tool's own
  `Err`. `ErasedToolFn` is `Arc`-wrapped with a documented rationale
  (cheap per-sub-agent clone of the tool list, `tool.rs:351-356`).
- **`ToolDetails` is a neutral, well-justified data contract
  (`tool.rs:57-177`).** The "closed enum keyed by rendering *shape*, not per
  tool" decision is explicitly documented, with the `Json(Value)` escape
  hatch and a stated graduation rule. It imports no UI crate — it's pure
  data with serde derives — so the agent layer carries zero UI concern while
  still giving the TUI a structured payload. Forward/backward serialization
  compatibility is handled deliberately (`skip_serializing_if` +
  `#[serde(default)]` on the optional Bash truncation fields, with the
  reason stated), and every variant's round-trip framing is locked by
  `tool_details_round_trips_each_variant` (`tool.rs:488`).
- **`AgentMessage` is a disciplined extension of the wire layer with no
  duplication (`message.rs`).** It wraps `Message` rather than re-declaring
  any wire type; `#[serde(transparent)]` + `untagged` make the on-disk shape
  a *bare* wire message (no redundant `kind` tag), so the conversion to/from
  what an SDK expects is lossless and zero-overhead. The single-variant enum
  is honestly explained as a forward-compat seam, the disambiguation
  contract for future variants ("each must carry its own discriminator,
  typically a distinct `role`") is spelled out, and the legacy-`"kind"`
  tolerance is both documented and tested (`message.rs:129`). `as_wire`
  returns `Option` precisely so future non-wire variants compose. This is a
  model shared-contract type.
- **The `Tool` ↔ `ToolDefinition` duplication does not originate here.** The
  contract layer's canonical shape is `ErasedToolDefinition`
  (name/description/`input_schema`/execution_mode/func). It maps 1:1 onto the
  wire `UnifiedToolDefinition` (name/description/`parameters`). The redundant
  `aj_models::tools::Tool` hop (AG1 Major / M1 Major) is introduced by the
  runtime's `with_provider` (`lib.rs:167`), not by these types — so a fix can
  drop the `tools::Tool` round-trip without touching the tool contract.
- **`types.rs` is a tidy display-data module.** `TokenUsage`/`SubAgentUsage`/
  `UsageSummary` are plain serializable snapshots with one of the clearest
  contract docs in the crate: the before-vs-after-add accumulator semantics
  (`types.rs:14-31`) precisely specify when the snapshot is taken relative to
  the running total, removing the usual ambiguity around usage counters.
- **Boundary is clean.** None of the three files import a UI, persistence,
  or `aj-conf` symbol; their only non-`std`/`serde` edge is `aj-models`
  (`UserContent`, `Message`) plus `schemars`. The shared truncation types
  (`BashStreamTruncation`, `TruncationCause`) live here as the persisted
  source of truth and are re-imported by `aj-tools::truncate`
  (`truncate.rs:33`) — sharing in the correct direction (tools depend on the
  contract, not vice versa).

## Boundary & architecture notes

The contract types respect the intended graph: `aj-agent` sits above
`aj-models`, and these three files only reach down to it. The
direction of type sharing is right — `aj-tools` imports `TodoItem`,
`TruncationCause`, `BashStreamTruncation`, `ExecutionMode`, and the tool
trait *from* `aj-agent`, never the reverse — so the contract layer is the
authoritative home for these shapes. `TodoItem` is intentionally dual-use:
it derives `JsonSchema` (it's the `todo_write` tool's input element) and is
also the `ToolDetails::Todos` render payload; that's a justified reuse, not
a leak.

The `aj-conf` edge AG1 flagged as Critical lives entirely in `lib.rs` and
does **not** touch these contract files — a point in favor of decoupling
`AgentEnv` from the runtime: the public *contract* surface is already free
of `aj-conf`.

The unresolved cross-boundary item for synthesis is the three-representation
tool-definition path (`ToolDefinition` trait → `ErasedToolDefinition` →
`tools::Tool` → `UnifiedToolDefinition`). AG2 confirms the contract end is
clean; the redundant middle hop is the runtime's to remove.

## Test assessment

In-module `#[cfg(test)]` per convention, exercising serialization contracts
at the boundary:

- **`tool.rs` `tool_details_round_trips_each_variant`** locks the
  `{"kind": "..."}` framing for all seven `ToolDetails` variants — the exact
  contract `aj-session` persistence and the TUI depend on. Good boundary
  test. `execution_mode_default_is_parallel` pins the documented default.
- **`message.rs`** has the two right tests: the bare-wire round-trip
  (asserts *no* `kind` tag leaks) and the legacy-`"kind": "wire"`
  tolerance. Together they pin the lossless-flatten and backward-compat
  contracts that are the whole reason the wrapper exists. Exemplary.

Gaps (see findings): `derive_schema`'s schema-shaping guarantees
(strip `title`, ensure `properties`/`required`) are untested though they
face the provider wire format; the `ToolDetails::Bash` legacy-payload
deserialization (missing truncation fields) is asserted in prose but not in
a test, unlike the analogous `AgentMessage` legacy test. `types.rs` has no
tests, which is defensible for plain data structs, but the documented
"snapshot taken before the add" accumulator semantics are a real contract
that lives in `lib.rs` and is worth one assertion there (out of AG2 scope —
note for AG1 follow-up).

No flakiness risk: all tests are pure serde round-trips, no clock, network,
or filesystem.

## Cross-cutting themes to bubble up

- **anyhow in lib crates (CONFIRMED, new locus — the public tool
  contract).** `ToolDefinition::execute` and `ToolContext::spawn_agent`
  return `anyhow::Result`, so the entire downstream tool ecosystem inherits
  `anyhow`. Broader than AG1's `TurnError` because it's the
  most-implemented-against trait in the workspace. Fold into the one
  workspace anyhow decision.
- **Contract docs that describe unimplemented behavior (NEW/related to AG1
  dead-surface).** `ToolContext::emit_update`'s doc promises a
  `ToolExecutionUpdate` bus emit the runtime never performs; this is the
  *root* of AG1's dead `ToolExecutionUpdate` variant. Synthesis should pair
  the two: a method whose contract is a no-op plus an event variant that's
  never emitted plus frontend code that handles it — one feature half-wired
  across three layers.
- **Chronology / `docs/aj-next-plan.md §N` references in comments
  (CONFIRMED, recurring).** Same theme as AG1: `tool.rs` and `message.rs`
  module docs cite plan-doc section numbers and narrate an "upcoming"
  migration that has already landed. The forward-compat rationale for
  `AgentMessage` is worth keeping as steady-state design prose.
- **Contract-by-convention id typing (NEW, minor).** Raw `usize` agent ids
  on `SpawnedAgent`/`SubAgentReport` are tied to `AgentId::Sub(_)` only by
  comment. Synthesis may want a single newtype for sub-agent ids across the
  tool and event boundaries.
- **What held up clean (worth noting positively):** the tool trait erasure
  boundary, `ToolDetails` as a UI-neutral data contract, and `AgentMessage`
  as a no-duplication wire-extension seam are all strong, replicable
  patterns. The duplication AG1/M1 chased is confirmed to *not* originate in
  the contract layer.
