# Audit findings — aj-tools-framework

- **Step:** TO1
- **Date:** 2026-06-02
- **Audited commit:** 5424919
- **Scope:** `src/aj-tools/src/lib.rs`, `src/aj-tools/src/tools.rs`,
  `src/aj-tools/src/truncate.rs`, `src/aj-tools/src/sanitize.rs`,
  `src/aj-tools/src/image.rs`, `src/aj-tools/src/testing.rs`,
  `src/aj-tools/Cargo.toml` (incl. the in-module `#[cfg(test)]` suites in
  `truncate.rs`, `sanitize.rs`, and `image.rs`).

## Summary

The framework/support layer of `aj-tools` is in good shape and the standout
themes are positive. The tool registry (`get_builtin_tools`) is a clean,
flat list that consumes the `aj-agent` contract through the
`Into<ErasedToolDefinition>` blanket impl with no local re-wrestling of the
trait — the `Tool` ↔ `ToolDefinition` duplication AG1/M1 flagged lives
entirely upstream in the runtime, not here. The three support utilities
(`truncate`, `sanitize`, `image`) are cohesive single-responsibility modules
with strong, well-documented contracts and the best happy-path-plus-edge-case
test coverage seen so far in the audit. Crucially, the M2 "truncate slices
bytes assuming ASCII" concern does **not** reproduce here:
`truncate_head`/`truncate_tail` operate on whole `\n`-delimited lines and the
single partial-line path snaps to a UTF-8 code-point boundary
(`take_last_bytes_utf8`), with a dedicated multibyte test.

The findings are localized: the disabled-tools filter is duplicated across
three binary call sites rather than living behind one seam; `testing.rs` is
an unconditionally-compiled `pub mod` (a test harness shipped into the
production API surface, the same over-broad-pub-surface theme as aj-models
`scripted`); a "SAFETY:" comment sits on a safe slice; and there are a couple
of small dead-config / unused-dependency items. No critical issues. The
boundary direction is correct throughout.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 4 | 2 |

## Findings

### [Major][Boundaries] `testing.rs` is an unconditionally-compiled `pub mod` — a test harness shipped into the production API — `src/aj-tools/src/lib.rs:17`, `src/aj-tools/src/testing.rs:19`
**What:** `lib.rs:17` declares `pub mod testing;` with no `#[cfg(test)]` or
feature gate, so `DummyToolContext` (a no-op `ToolContext` that returns errors
from `spawn_agent`, discards `emit_update`, and hands out a never-firing
cancellation token) is compiled into every release build and exported as part
of `aj-tools`'s public API. Its own doc says it exists "so individual tools
can be exercised from CLI bins and integration tests without standing up a
full agent runtime" (`testing.rs:3-5`) — i.e. it is a test fixture. The
`tempfile` crate is even a *non-dev* dependency partly to support it
(`Cargo.toml:17`), and `tokio` features needed only by the harness/tests are
pulled in.
**Why it matters:** Same over-broad / test-only public surface theme the
audit flagged for `aj-models`'s `scripted`. A test double in the shipped API
lets production code (or a downstream crate) accidentally wire a `ToolContext`
that silently drops `emit_update` and never cancels — exactly the kind of
boundary leak the rubric calls out (persistence/test scaffolding bleeding into
runtime). It also inflates the release dependency graph (`tempfile` as a hard
dep) for code that only runs under test.
**Suggested action:** Gate the module behind `#[cfg(any(test, feature =
"testing"))]` (or a dedicated `test-support` feature that integration tests
and CLI bins opt into), and move `tempfile` to `dev-dependencies` unless a
non-test code path actually needs it (see the dependency finding below).
**Effort:** S

### [Minor][Simplicity] Disabled-tools filtering is duplicated across three binary call sites instead of living behind one seam — `src/aj-tools/src/lib.rs:52-59`, `src/aj/src/modes/interactive.rs:175-179,224-228`, `src/aj/src/modes/print.rs:143-148`
**What:** `get_builtin_tools` returns the full catalog and explicitly defers
filtering: "The binary further filters this list against any tools the user
has disabled before handing it to the agent" (`lib.rs:54-56`). That
`tools.retain(|tool| !config.disabled_tools.contains(&tool.name))` filter is
then copy-pasted verbatim at three sites in the binary (interactive build
path ×2, print mode ×1). The disabled-tools *name set* is the contract that
both `aj-tools` (catalog) and `aj-agent` (sub-agent inheritance,
`lib.rs:1648-1654`) reason about, but the actual application of it is
scattered convention rather than a function.
**Why it matters:** Three independent copies of a filter that must agree drift
easily (e.g. one site forgetting to filter, or applying it before vs. after
`BuiltinToolOptions` are honored). The natural home for "build the catalog,
minus disabled names" is `aj-tools` itself, which already owns the catalog and
documents the filtering contract. Keeping the seam in `aj-tools` would also
let the sub-agent path reuse the same predicate instead of the runtime
hand-rolling `filter(|tool| tool.name != "agent")` plus carrying
`disabled_tools` separately.
**Suggested action:** Add `get_builtin_tools(options, disabled: &[String])`
(or a thin `filter_disabled` helper) in `aj-tools` and call it from all three
binary sites. Decide with the user whether the sub-agent inheritance path in
`aj-agent` should consume the same predicate.
**Effort:** S

### [Minor][Contracts] `truncate_tail`'s `truncated_by` disambiguation is enforced by an ad-hoc post-pass condition, not by the contract — `src/aj-tools/src/truncate.rs:258-263`
**What:** After the backward walk, `truncate_tail` re-attributes the cause:
`if kept.len() >= max_lines && running_bytes <= max_bytes &&
partial_line.is_none() { truncated_by = TruncatedBy::Lines; }`. The loop
already sets `truncated_by` on each break, so this is a corrective second
pass for the "both budgets tight" tie. The exact precedence rule (lines wins
the tie when the byte total is still under cap) is described in the inline
comment but not in the `TruncationResult::truncated_by` doc
(`truncate.rs:48`), and there is no test that exercises a simultaneous
line+byte boundary to pin which cause wins.
**Why it matters:** `truncated_by` feeds the model-facing truncation marker
and the `ToolDetails` payload the UI renders; the tie-break is a real contract
that callers (bash's `finalize_stream`, `truncate.rs:537-543`) layer further
logic on top of. A behavioral contract enforced by a follow-up `if` with no
test is the kind of invariant-by-convention the rubric flags.
**Suggested action:** State the tie-break precedence on the `truncated_by`
doc, and add a `truncate_tail` test where the kept content hits the line cap
and the byte total equals `max_bytes`, asserting `TruncatedBy::Lines`.
**Effort:** S

### [Minor][Testing] `resize_image`'s `None` (no encoding fits) bailout and `passthrough_image` are untested — the only error/degenerate paths in `image.rs` — `src/aj-tools/src/image.rs:283-308,328-343`
**What:** The resize tests cover passthrough, shrink, PNG-preferred, and
JPEG-fallback, but never the terminal failure path: the loop that shrinks by
0.75 until `(1,1)` and returns `None` when nothing fits
(`image.rs:283-308`). `passthrough_image` (`image.rs:328-343`), the
`image_auto_resize = false` path, also has no test — neither the success case
(dimensions decoded, `was_resized = false`) nor the `None`-on-undecodable
case. `detect_mime_type` is tested well, but `resize_image` on malformed /
undecodable bytes (the `ImageReader … .decode().ok()?` early return,
`image.rs:225-229`) is likewise unexercised.
**Why it matters:** These are the contract's failure modes that callers branch
on ("emit a textual 'image omitted' note in place of the attachment",
`image.rs:206-208`). Happy-path-only coverage on the paths that decide whether
an attachment is dropped is exactly the coverage gap the rubric targets, and
`resize_image` returning `None` vs. a bad payload is the difference between a
clean omission note and a wire rejection.
**Suggested action:** Add a test feeding `resize_image` a tiny `max_bytes`
that no `(1,1)` encoding can meet to assert `None`; add a `passthrough_image`
success test and a `passthrough_image(b"not an image")` → `None` test.
**Effort:** S

### [Minor][Dependencies] `tempfile` is a hard (non-dev) dependency but is only used by the test-only harness and tests — `src/aj-tools/Cargo.toml:17,23`
**What:** `tempfile` appears in both `[dependencies]` (`Cargo.toml:17`) and
`[dev-dependencies]` (`Cargo.toml:23`). Within the framework scope its only
uses are the `#[cfg(test)]` suite in `image.rs` (`NamedTempFile` at
`image.rs:774`) and indirectly the test harness story. `bash.rs` does use
`tempfile` for spill files (non-test), so the dep is not *entirely* test-only
— but the duplicate dev-dependency line is redundant given the hard dep, and
the hard dep is partly justified by `testing.rs` being un-gated (see the
`testing.rs` finding).
**Why it matters:** Dependency-hygiene dimension: a dep listed in both tables
is a smell, and a non-dev dep that exists largely to support an un-gated test
harness conflates the production and test graphs. If `testing.rs` is gated and
`bash` remains the sole non-test user, the hard dep is justified and the
`[dev-dependencies]` line should be dropped.
**Suggested action:** Remove the redundant `tempfile` entry from
`[dev-dependencies]` (the hard dep already covers tests). Re-evaluate the hard
dep's justification once `testing.rs` is gated; keep it for `bash`'s spill
file.
**Effort:** S

### [Nit][Comments] `take_last_bytes_utf8` carries a `// SAFETY:` comment on a safe slice — `src/aj-tools/src/truncate.rs:306-307`
**What:** The function snaps `start` to a UTF-8 boundary and returns
`s[start..].to_string()` — a perfectly safe, panic-checked string slice. The
preceding `// SAFETY: we positioned start on a UTF-8 boundary in valid
UTF-8.` comment uses the convention reserved for `unsafe` blocks, but there is
no `unsafe` here (Rust would panic, not invoke UB, on a bad boundary).
**Why it matters:** The `SAFETY:` prefix signals an `unsafe` invariant to
readers and to lints/tooling that grep for unjustified `unsafe`. Mislabeling a
safe path dilutes that signal and slightly misleads about the failure mode (a
wrong `start` panics; it is not UB).
**Suggested action:** Reword to a plain nota bene, e.g. "`start` is on a
code-point boundary, so this slice never panics." Drop the `SAFETY:` prefix.
**Effort:** S

### [Nit][Comments] `BuiltinToolOptions` doc narrates future field-addition policy (forward-looking framing) — `src/aj-tools/src/lib.rs:33-36`
**What:** The struct doc says "Today scopes only image-related flags; new
fields will be `Default`-derived so callers can extend without churning every
call site." This is the chronology/forward-looking comment pattern the audit
has flagged elsewhere (AG1/AG2 `docs/aj-next-plan.md §N` references):
describing what the type does "today" and what "will" happen rather than the
steady-state contract.
**Why it matters:** Comments should stand on their own from the current code.
"Today only X; later Y" goes stale and reads as a roadmap note. (Note: the
struct does **not** actually derive `Default` field-additions safely today —
adding a non-`Default` field would still churn call sites — so the promise is
also slightly aspirational.)
**Suggested action:** Restate as steady-state: "Cross-cutting settings the
binary feeds into builtin tool construction; currently image-related flags."
Drop the "today/will" framing.
**Effort:** S

## What's good

- **The tool registry is a clean, flat seam (`lib.rs:52-69`).**
  `get_builtin_tools` is a single `vec![...]` of `T.into()` conversions
  through `aj-agent`'s `From<T> for ErasedToolDefinition` blanket impl. It
  does **not** reproduce the `Tool` ↔ `ToolDefinition` duplication AG1/M1
  chased — `aj-tools` produces `ErasedToolDefinition` directly; the redundant
  `tools::Tool` hop is introduced upstream by the runtime's `with_provider`
  (`aj-agent/src/lib.rs:167`), confirming AG2's conclusion that the duplication
  originates in the runtime, not the tool layer. The crate also does not
  wrestle with the trait's `anyhow` contract: it simply implements `execute`
  per-tool and converts; the anyhow-in-lib concern is inherited from the
  `aj-agent` trait (AG2), not re-introduced here.
- **`truncate.rs` handles UTF-8 / multibyte correctly — the M2 concern does
  not reproduce.** `truncate_head` joins whole `\n`-split lines and never
  slices mid-byte; `truncate_tail`'s only sub-line path
  (`take_last_bytes_utf8`, `truncate.rs:294-308`) snaps forward to a
  code-point boundary, and `take_last_bytes_utf8_respects_char_boundary`
  (`truncate.rs:432-444`) pins it with a 2-byte `é`. The dual line+byte
  budget, the "first line exceeds limit → empty + flag" head behavior, and the
  "partial trailing line preserved" tail behavior are all documented as
  contracts on `TruncationResult` and tested. Strong, replicable module.
- **`sanitize.rs` is a cohesive, well-tested ingress filter.** One public
  function with a precise documented contract (which C0 controls survive, the
  CSI/OSC/APC/PM/SOS shapes recognized, the truncated-sequence-consumes-rest
  rule, the FFF9–FFFB drop), a fast path for already-clean ASCII, and a test
  per branch including realistic mixed cargo-style output. The "drop a stray
  ESC rather than leak half a control sequence" decision is exactly the kind of
  non-obvious choice the guidance wants called out, and it is.
- **`image.rs` parses malformed input defensively — no panics on bad bytes.**
  The MIME sniffer and both EXIF parsers (`parse_jpeg`, `parse_webp`,
  `parse_tiff`) use `get(..)`/`checked_add`/`try_from(..).ok()?` throughout and
  return `None`/`1` on any malformed header rather than indexing-panic; the
  animated-PNG walk guards against zero/backward advances
  (`image.rs:138-143`). `usize -> f64` and `as` casts are confined behind
  `#[allow(clippy::as_conversions)]` with a documented precision rationale.
  EXIF orientation is read, applied, and reflected in reported dimensions, with
  endianness and rotate-90/180 dimension-swap tests.
- **Module boundaries and dependency direction are correct.** Every support
  module has a single clear responsibility; `truncate.rs` imports the persisted
  `TruncationCause` *from* `aj-agent` (sharing in the right direction,
  confirming AG2's note) and aliases it locally; no module reaches into UI or
  persistence. `lib.rs` documents that `aj-tools` is "wire-only" and that
  rendering of `ToolDetails` lives in the binary — and the code honors it.

## Boundary & architecture notes

Dependency direction matches `CLAUDE.md`: `aj-tools` depends on `aj-agent`
(tool trait, `ToolContext`, `ToolDetails`, `TruncationCause`) and `aj-models`
(`UserContent`, `ImageContent`), and nothing reaches back up into the binary
or the TUI. The catalog produces `ErasedToolDefinition` and stops there;
filtering and rendering are the binary's job, which the module docs state
explicitly and the code respects.

Two surface items to verify in synthesis: (1) `testing.rs` as an un-gated
`pub mod` is the same over-broad public-surface pattern as `aj-models`'s
`scripted` — synthesis should collate all test-support modules shipped in
production APIs and decide on a uniform `testing`/`test-support` feature
convention. (2) The disabled-tools filter is applied by convention at three
binary sites plus a fourth bespoke filter in the runtime's sub-agent path;
synthesis may want a single owner for "the active toolset" predicate.

## Test assessment

The in-module `#[cfg(test)]` suites are a high point: they exercise the public
contract (not internals), cover edge and boundary values rather than just the
happy path, and use readable, self-documenting fixtures. `truncate.rs` tests
both caps independently, the first-line-overflow flag, the partial-tail
preservation, and the multibyte boundary. `sanitize.rs` has a test per escape
family plus truncation and a realistic blend. `image.rs` builds real PNG/JPEG
fixtures and synthesizes TIFF/APP1 EXIF blobs in both endiannesses.

Gaps (see findings): `image.rs` never exercises the `resize_image` →
`None` bailout, `passthrough_image`, or undecodable input; `truncate_tail`
has no simultaneous-line+byte-boundary test pinning the `truncated_by`
tie-break. No wall-clock, network, or ordering coupling anywhere in the
framework tests — the one filesystem touch (`detect_mime_type_from_file`) uses
`tempfile` and a clearly-nonexistent path, so no flakiness risk. Note
`testing.rs`'s `DummyToolContext::default` reads `std::env::current_dir`,
which is process-global state a test could race on, but it is a documented
override point and not a clock/ordering hazard.

## Cross-cutting themes to bubble up

- **Over-broad / test-only public surface (CONFIRMED, new locus).**
  `testing.rs` ships `DummyToolContext` into the production API un-gated, the
  same pattern as `aj-models`'s `scripted`. Synthesis should propose a uniform
  `testing`/`test-support` feature gate across crates.
- **anyhow in lib crates (CONFIRMED upstream, not re-introduced here).**
  `aj-tools`'s tools return `anyhow::Result` because the `aj-agent`
  `ToolDefinition` trait demands it (AG2). The framework layer does not add a
  new anyhow locus — it inherits the one decision. Fold into the workspace
  anyhow call.
- **Multibyte/Unicode truncation (CONFIRMED clean here, contrast with M2).**
  Unlike `aj-models::transform::truncate` (M2: byte-slices assuming pre-ASCII
  sanitization), `aj-tools::truncate` is line-oriented and the one sub-line
  path is UTF-8-boundary-safe and tested. Worth recording that the workspace
  has two truncation implementations with different safety contracts — a
  synthesis-level note on whether they should converge.
- **Invariant-/contract-by-convention (CONFIRMED, minor).** The disabled-tools
  filter (three duplicated call sites) and `truncate_tail`'s tie-break post-pass
  are behaviors enforced by convention/inline comment rather than by a single
  function or a documented + tested contract.
- **What held up clean (worth replicating):** the three support utilities are a
  model for cohesive single-responsibility modules with documented contracts
  and edge-case-first tests; the defensive `get(..)/checked_add/ok()?` parsing
  in `image.rs` is the right pattern for untrusted byte input.
