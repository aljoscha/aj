# Audit findings — aj-models-core

- **Step:** M1
- **Date:** 2026-06-02
- **Audited commit:** b415d89
- **Scope:** `src/aj-models/src/lib.rs`, `provider.rs`, `registry.rs`,
  `refresh.rs`, `types.rs`, `errors.rs`, `tools.rs`, `Cargo.toml` (incl.
  the in-module `#[cfg(test)]` suites in `provider.rs`, `registry.rs`,
  `refresh.rs`, `types.rs`, `errors.rs`).

## Summary

The core of `aj-models` is a strong, well-documented wire layer. The
`Provider` trait is a clean two-method seam with a uniform
stream-shaped error contract, the unified message/types module stays
genuinely wire-only (with one deliberate UI-only `details` escape hatch
that is correctly excluded from provider conversion), and the registry /
refresh code is cohesive, defensive, and unusually well-tested for an
offline-capable catalog with hand-curated Codex entries. The main
blemishes are localized: a second tool-definition type (`tools::Tool`)
that duplicates `types::ToolDefinition` and forces `aj-agent` to
round-trip between them, two broken intra-doc links pointing at
`crate::errors::*` for types that live in `crate::types`, the
`async-stream` dependency declared but unused anywhere in the workspace
(the same dead-dep theme confirmed in `openai-sdk`), and `refresh.rs`
leaning on `anyhow` inside a library crate. No correctness, data-loss,
or secret-handling risks; the boundaries hold.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 4 | 3 |

## Findings

### [Major][Boundaries] `tools::Tool` duplicates `types::ToolDefinition`, forcing a redundant round-trip in `aj-agent` — `src/aj-models/src/tools.rs:4`, `src/aj-models/src/types.rs:293`
**What:** The crate exposes two tool-description types. `types::ToolDefinition`
(`types.rs:293`) is the unified type carried inside `Context.tools` and
the only one the providers actually consume. `tools::Tool` (`tools.rs:4`)
is a near-identical struct — `name`, `description`, `input_schema`
(vs. `parameters`), plus an `r#type: Option<String>` field — that no
provider in `aj-models` reads. `aj-agent` holds `Vec<Tool>`
(`aj-agent/src/lib.rs:64`), builds it from its erased tool definitions
(`lib.rs:169`, always setting `r#type: None`), then at inference time
maps it field-by-field back onto `ToolDefinition` to populate `Context`
(`lib.rs:1351`). So the data is `ErasedToolDefinition → Tool →
ToolDefinition`, with `Tool` adding no information the final shape uses
and the `r#type` field never being read by anyone.
**Why it matters:** Two public types for one concept is exactly the kind
of boundary smell the rubric flags: the consumer must know which one each
layer wants and translate between them, the `input_schema`/`parameters`
naming split invites confusion, and `tools.rs` is an undocumented
single-struct module with no module doc explaining why it exists
alongside `ToolDefinition`. The `r#type` field is dead surface.
**Suggested action:** Collapse the two onto one type. Either have
`aj-agent` build `ToolDefinition` directly (dropping `tools::Tool` and
the `tools` module), or, if `tools::Tool` exists to mirror a specific SDK
wire shape, document that intent and the role of `r#type`. Decide with
the user since the fix spans the `aj-models`/`aj-agent` boundary.
**Effort:** M

### [Minor][Dependencies] `async-stream` is declared but never used anywhere in the workspace — `src/aj-models/Cargo.toml:9`
**What:** `async-stream.workspace = true` is in `[dependencies]`, but
`rg async_stream src` returns nothing across the entire workspace — not in
the M1 scope, not in streaming/transform, not in the providers. This is
the same dead dependency flagged for `openai-sdk` (which also declared an
unused `async-stream`), confirmed to recur here.
**Why it matters:** Unused dependency: extra compile time and a
misleading signal that streaming is built on `async_stream` when it is
not. Dependency-hygiene miss the workspace lints don't catch.
**Suggested action:** Remove `async-stream` from `Cargo.toml`. (Verify it
isn't relied on transitively by the M2 streaming step before removal, but
the grep across `src` shows no direct use.)
**Effort:** S

### [Minor][Comments] Two intra-doc links point at `crate::errors::*` for types that live in `crate::types` — `src/aj-models/src/types.rs:503,620`
**What:** The `ApiKeyResolver::call` doc (`types.rs:503`) links
`[crate::errors::AssistantError]` and the `resolve_api_key` doc
(`types.rs:620`) links `[crate::errors::ErrorCategory::Auth]`. Both
`AssistantError` and `ErrorCategory` are defined in `crate::types`
(`types.rs:212,236`); `errors.rs` only *imports* them and exports no
re-export (`rg "pub use" errors.rs` is empty). The links resolve to
nonexistent paths.
**Why it matters:** Broken `rustdoc` intra-doc links (these would warn
under `-D rustdoc::broken_intra_doc_links` if doc-lints were enabled) and
they misdirect a reader to the wrong module for the canonical definition.
**Suggested action:** Point the links at `crate::types::AssistantError`
and `crate::types::ErrorCategory::Auth` (or `Self`-relative, since
they're in the same module).
**Effort:** S

### [Minor][Errors] `refresh.rs` uses `anyhow` throughout a library crate instead of a `thiserror` error type — `src/aj-models/src/refresh.rs:17,159,166`
**What:** The public refresh API (`refresh_user_cache`,
`refresh_user_cache_from`) returns `anyhow::Result` and builds errors
with `bail!`/`context` (`refresh.rs:17,196,389`). Per `CLAUDE.md` and the
rubric, library crates define error types with `thiserror`; `anyhow` is
reserved for the top-level `aj` binary. `anyhow` is a real dependency of
`aj-models` (`Cargo.toml:7`) and the refresh path is its primary use in
the M1 scope. This is the same "lib crate leaning on anyhow" theme the
SDK audits flagged for the non-streaming client methods, here in the
catalog-refresh surface.
**Why it matters:** The refresh boundary surfaces opaque `anyhow::Error`
to its CLI caller, so callers can't programmatically distinguish a
network failure from a non-2xx response from a write error — they can
only render the string. It also widens the crate's dependency on
`anyhow` beyond the binary.
**Suggested action:** Define a `thiserror` `RefreshError` enum (e.g.
`Fetch`, `HttpStatus { status, body }`, `Parse`, `Write`,
`NoCachePath`) and return it from the two public entry points. Keep the
internal `with_context` ergonomics if helpful, but present a typed error
at the boundary. Coordinate with the cross-SDK error-type decision in
synthesis.
**Effort:** M

### [Minor][Contracts] `parse_retry_after` clamps a past HTTP-date to `0` but a future date that overflows `i64` ms is undocumented; fractional-seconds `as u64` cast noted but date path silent — `src/aj-models/src/errors.rs:379`
**What:** For the HTTP-date form, a date in the past yields a negative
`num_milliseconds()` which `u64::try_from(...).unwrap_or(0)` maps to
`Some(0)` — a reasonable "retry now" but undocumented at the function's
contract (the doc only says "Returns `None` if missing or unparseable").
A caller can't tell `Some(0)` (past date) apart from `Some(0)` (literal
`"0"` seconds). Minor, but the boundary contract claims only seconds and
date forms without stating the past-date collapse.
**Why it matters:** A small contract gap on an error-recovery helper that
the retry path keys off; the behavior is fine but enforced by convention,
not documented.
**Suggested action:** Add one sentence to the doc: "an HTTP-date already
in the past returns `Some(0)` (retry immediately)." The numeric `as`
casts are already justified with the `#[allow]` and a comment.
**Effort:** S

### [Nit][Style] `provider.rs` module doc narrates incremental delivery ("plug in here as they land in §6 and §7; until each one arrives") — `src/aj-models/src/provider.rs:64`
**What:** The `provider_for` doc says concrete providers "plug in here as
they land in §6 and §7; until each one arrives the top-level dispatch
functions surface the missing provider as an error." All four providers
*are* wired in (`provider.rs:76-79`), so this is chronology framing the
rubric calls out: it describes a past/incremental state rather than the
current contract.
**Why it matters:** Comments should stand on their own from the current
code; "until each one arrives" reads as stale once every arm is
populated.
**Suggested action:** Restate as the steady-state contract: "Returns
`None` for an unrecognized `api`; the top-level dispatch functions turn
that into an `AssistantMessageEvent::Error` so callers always observe a
uniform stream shape."
**Effort:** S

### [Nit][Comments] `provider.rs` test comment "once a provider is wired in" / "once real providers land" is stale chronology — `src/aj-models/src/provider.rs:201,268`
**What:** The `EchoProvider` doc (`provider.rs:201`) says it verifies the
dispatch path "once a provider is wired in," and the
`provider_trait_drives_dispatch_when_implemented` test
(`provider.rs:268`) says "once real providers land." Real providers are
wired in (and a dedicated test for the Codex dispatch exists at
`provider.rs:288`). Same chronology smell as the module doc.
**Why it matters:** Minor; the tests are good but the comments date
themselves.
**Suggested action:** Reword to the present tense (e.g. "guards against
drift in the trait signature").
**Effort:** S

### [Nit][Comments] Staleness warning message names a CLI command (`aj update-models`) that doesn't match the spec's `aj models update` — `src/aj-models/src/registry.rs:305`
**What:** `maybe_warn_stale` logs "run `aj update-models` to refresh",
while `CLAUDE.md` documents the command as `aj models update` and
`refresh.rs:1` refers to the flow as `aj update-models`. The user-facing
string and the canonical command name disagree somewhere.
**Why it matters:** A user copy-pasting the suggested command may hit an
unknown subcommand. Low severity but user-visible.
**Suggested action:** Reconcile the command name across the warning,
`refresh.rs` docs, and `CLAUDE.md` (confirm the actual `clap` subcommand
in the A1 step and use that spelling).
**Effort:** S

## What's good

- **`Provider` trait as a seam (`provider.rs:35`).** Exactly two methods
  (`stream` / `stream_simple`), `Send + Sync`, stateless by design with
  per-call auth/HTTP knobs flowing through `StreamOptions` — the module
  doc states that lifecycle rationale explicitly. The uniform terminal
  contract (every stream ends with exactly one `Done` or `Error`, drop =
  cancel) is documented on the trait and exercised by the
  unknown-API-emits-error tests. This is a well-judged abstraction, not a
  one-impl no-op trait.
- **Unified types stay wire-only (`types.rs`).** No UI/persistence
  concepts leak into the message types. The one apparent exception —
  `ToolResultMessage.details` (`types.rs:131`) — is explicitly documented
  as "preserved for UI/logs but never sent to the provider" and serde-
  skipped when absent, with a round-trip test pinning that contract
  (`types.rs:870`). The non-serializable `OnPayload`/`ApiKeyResolver`/
  `CancellationToken` fields are correctly `#[serde(skip)]` with `Debug`
  impls that don't try to format the closures, and there are tests
  proving the skip doesn't break round-trip while the callback stays
  invokable.
- **`ApiKeyResolver` design (`types.rs:481`).** Clean newtype over an
  `Arc<dyn Fn -> BoxFuture>` with a documented preference order
  (`resolve_api_key`), the per-call invocation invariant, and a focused
  async test suite covering resolver-wins, static fallback, neither-set
  error, verbatim error propagation, and per-call invocation via a shared
  counter. Good boundary testing of an auth-refresh seam.
- **Error classification module (`errors.rs`).** The `classify_*`
  functions are a cohesive, provider-independent classification layer with
  per-provider tables that cite the spec sections, a defensive
  overflow-regex fallback gated behind exclusion patterns (so "rate limit
  … too many requests" doesn't false-positive), and a thorough truth-table
  test suite including the silent-overflow (usage > window on `Stop`)
  branch and `is_retryable` across every category. `RegexSet`s are built
  once via `OnceLock`. This is the right shape for the cross-provider retry
  contract.
- **Registry/refresh robustness (`registry.rs`, `refresh.rs`).**
  Offline-first (bundled seed always parses or panics as a build-time
  bug), user-cache parse failures degrade gracefully with a warning, the
  hand-curated Codex seed is spliced additively (cache wins on conflict),
  refresh writes atomically via temp-file + rename, and the diff/summary
  logic is deterministic (sorted output). The test suite covers filtering,
  default fallbacks, the additive splice, cross-run codex preservation,
  and validates the committed seed/overrides parse and that every override
  target matches a real seed entry. This matches the "generate from an
  authoritative source, don't ship a static snapshot" guidance well.
- **`calculate_cost` cast (`registry.rs:354`).** The `u64 → f64`
  conversion is justified with a precise comment (exact below 2^53, token
  counts stay below) and an `#[allow(clippy::as_conversions)]` rather than
  a silent cast.

## Boundary & architecture notes

Dependency direction is correct for the M1 scope: the core modules depend
on `serde`/`serde_json`/`regex`/`chrono`/`tracing`/`reqwest`/`tempfile`/
`futures`/`tokio-util` and on the in-crate `streaming`/`types`/`registry`
modules, with no `aj_*` edges — `aj-models` sits below `aj-agent`/
`aj-session` exactly as the `CLAUDE.md` graph intends. The provider
SDKs (`anthropic-sdk`, `openai-sdk`) are consumed only by the adapter
modules audited in M3/M4, not by the M1 core.

The one cross-layer wrinkle is `tools::Tool` (Major finding above): it's a
public type in `aj-models` that exists purely so `aj-agent` can hold a
list and then convert it to `ToolDefinition` — the seam would be cleaner
if `aj-agent` produced `ToolDefinition` directly. Worth confirming in
synthesis whether any other consumer (binary, session replay) relies on
`tools::Tool`'s shape; the M1 grep shows only `aj-agent`.

`errors.rs` deliberately has **no** crate-level `thiserror` error enum: it
is a classification layer producing `AssistantError` *values* that ride on
the stream, not a `Result<_, E>` boundary. That's the right call for the
streaming contract and is not a violation of the "lib crates use
thiserror" rule. The rule does bite `refresh.rs`, which has a real
`Result` boundary and uses `anyhow` (Minor finding). For synthesis: the
crate carries `anyhow` (`Cargo.toml:7`) and `thiserror` (used by
`oauth.rs`, audited in M5); resolving the refresh finding plus the
SDK-level `anyhow` findings should be considered together when deciding
whether `aj-models` can shed `anyhow`.

## Test assessment

Tests are in-module under `#[cfg(test)]` per convention and exercise the
right boundaries rather than internals:

- **provider.rs** tests dispatch behavior through the public `complete` /
  `complete_simple` entry points (unknown-API → `InvalidRequest` error
  with identity fields preserved; Codex API → routed to the codex provider
  and distinguished by the `Auth` category from a fast-fail auth check
  without a network call). The `EchoProvider` is a good trait-shape guard.
- **types.rs** round-trips every message/enum wire form, pins the
  lowercase `ThinkingLevel` serialization (including `xhigh`), proves the
  serde-skipped callback fields don't break round-trip, and covers the
  `details` omit-when-absent contract. The async `resolve_api_key` suite
  is strong.
- **errors.rs** is the standout: classification truth tables per provider,
  overflow-vs-exclusion regex behavior, silent-overflow, `Retry-After`
  integer/fractional/HTTP-date/empty cases, and the full `is_retryable`
  matrix.
- **registry.rs / refresh.rs** cover lookup/ordering, shallow-merge
  overrides (including unknown-target no-op), cost math, the additive
  codex splice and its non-codex filtering, cross-run codex preservation,
  and that the *committed* seed/overrides parse and cross-reference.

Notable gaps / risks:
- **No test drives a `Provider` impl through a non-terminal event path.**
  `EchoProvider` only emits a single `Done`; the documented invariant
  "terminates with exactly one of `Done`/`Error`" and the drain semantics
  in `drain` (`provider.rs:146`) aren't exercised against a multi-event
  stream here (likely covered in M2/M3/M4 — confirm in synthesis).
- **`load_real_registry_applies_overrides` and `load_surfaces_codex_models`
  call `ModelRegistry::load()`**, which reads `~/.aj/models.json` if it
  exists (`registry.rs:404`). On a developer/CI machine that happens to
  have a user cache, these tests read real filesystem state and could
  behave differently than on a clean machine — a latent
  environment-coupling/flakiness risk. The assertions are guarded with
  `if let Some(...)` so they degrade to no-ops rather than failing, but
  that also means they can silently stop testing anything. Consider
  exercising the override/codex application through
  `from_catalog_with_overrides` against the bundled seed instead of the
  HOME-dependent `load()`.
- `parse_retry_after`'s past-date → `Some(0)` branch is exercised only
  indirectly (the date test uses a future date); the documented-contract
  gap (Minor finding) means the past-date case is neither documented nor
  directly tested.

Fixtures are inline and readable; the refresh `FIXTURE` is a tidy,
self-documenting models.dev sample. No wall-clock coupling beyond the
deliberately-slack `parse_retry_after` future-date assertion.

## Cross-cutting themes to bubble up

- **Dead/unused declared dependency (CONFIRMED).** `async-stream` is
  declared in `aj-models/Cargo.toml` and unused workspace-wide — the same
  dead-dep `async-stream` flagged in `openai-sdk`. Synthesis should sweep
  all manifests for declared-but-unused deps.
- **`anyhow` in library crates (CONFIRMED, new locus).** Both SDKs split
  error types between `thiserror` and `anyhow`; here `refresh.rs` uses
  `anyhow` for a genuine `Result` boundary inside a lib crate. The
  workspace-wide decision on whether non-binary crates may use `anyhow`
  (and whether `aj-models`/the SDKs can drop the dep) should be made once
  in synthesis.
- **Duplicated type for one concept across a boundary (new).** `tools::Tool`
  vs. `types::ToolDefinition` is `aj-models`-internal duplication that
  forces a consumer round-trip — analogous in spirit to the SDKs' orphaned
  convenience surface but here it actively crosses the
  `aj-models`/`aj-agent` boundary. Worth checking whether other
  crate pairs carry similar near-duplicate "model" vs "wire" types.
- **Where wire-correctness lives (CONFIRMED).** As with the SDKs, the M1
  types rely on in-module serde round-trips plus the downstream
  `tests/roundtrip/` suite for full wire correctness; the placement is
  reasonable but the synthesis should note the consistent reliance on
  downstream round-trip tests across the stack.
- **Command-name drift (new, minor).** The staleness warning, `refresh.rs`
  docs, and `CLAUDE.md` disagree on the refresh subcommand name
  (`aj update-models` vs. `aj models update`); verify against the real CLI
  in A1 and unify.
