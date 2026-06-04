# Audit findings — aj-tools-impls

- **Step:** TO2
- **Date:** 2026-06-02
- **Audited commit:** 61b31b1
- **Scope:** `src/aj-tools/src/tools/agent.rs`, `src/aj-tools/src/tools/bash.rs`,
  `src/aj-tools/src/tools/read_file.rs`, `src/aj-tools/src/tools/write_file.rs`,
  `src/aj-tools/src/tools/edit_file.rs`, `src/aj-tools/src/tools/edit_file_multi.rs`,
  `src/aj-tools/src/tools/todo.rs` (incl. each file's in-module `#[cfg(test)]`
  suite).

## Summary

The individual tool implementations are in good shape: each tool maps cleanly
to one `ToolDefinition`, returns structured `ToolDetails` alongside wire content,
and consistently converts input-validation failures into recoverable
`is_error: true` outcomes rather than bubbling `Err` (so the model can correct
itself). The boundary between the tool layer and the runtime is respected
throughout — tools never reach back into the binary, and the dangerous concerns
(process-group teardown, output truncation, in-memory atomic edit, sub-agent
recursion bound) are handled deliberately and, in `bash.rs`, documented well.
Test coverage of error/edge paths is the strongest the audit has seen.

The findings cluster around three themes. First, **non-atomic file writes**:
`write_file`, `edit_file`, and `edit_file_multi` all `fs::write` truncate-in-place,
so a crash or full disk mid-write can leave a file partially written or empty —
the data-loss theme C1/SE1 flagged, now reproduced for model-driven mutation.
Second, **contract/description mismatches**: the `agent` tool's schema says it
"runs a single turn," but it drives the full multi-iteration agent loop with no
turn cap; and three file tools' descriptions don't mention that matching/replacement
are exact substring operations with non-overlapping count semantics. Third,
**duplication across the edit/write siblings**: `display_relative`, `error_outcome`,
the absolute-path guard, and the match-count-then-replace logic are copy-pasted
four to five ways. None of these are critical in isolation; the non-atomic write
is the highest-signal item because it is a real (if narrow) data-loss vector on a
tool the model invokes constantly.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 1 | 7 | 2 |

## Findings

### [Major][Contracts] File writes truncate in place rather than write-temp-then-rename — a crash or disk-full mid-write loses the prior file — `src/aj-tools/src/write_file.rs:98`, `src/aj-tools/src/edit_file.rs:136`, `src/aj-tools/src/edit_file_multi.rs:170`
**What:** All three mutating tools persist with a bare `fs::write(path, &content)`,
which opens the target with `O_TRUNC` and writes the buffer in place. If the
process is killed, the host loses power, the disk fills, or the write returns a
short/partial result midway, the file is left truncated or partially written —
the original contents are already gone. `edit_file_multi`'s module doc even
advertises an "all edits applied atomically" contract (`edit_file_multi.rs:16,48`),
but that atomicity is only about *validation* ordering (no disk touch until every
edit validates); the *write itself* is not atomic. `write_file` and `edit_file`
make no atomicity claim but inherit the same truncate-in-place hazard.
**Why it matters:** These tools are invoked constantly under model control on the
user's working tree. A failed write that empties or corrupts a source file is
real data loss with no recovery path (the `before` snapshot lives only in the
returned `ToolDetails`, not on disk). This is the same non-atomic-write theme the
audit flagged for `aj-conf` (C1) and `aj-session` (SE1), here reproduced on the
highest-frequency mutation surface. The contract doc claiming atomicity makes it
worse: a reader trusts a guarantee the `fs::write` does not provide.
**Suggested action:** Write to a sibling temp file in the same directory, fsync,
then `fs::rename` over the target (rename is atomic within a filesystem). Factor
this into one shared `write_atomic(path, bytes)` helper used by all three tools
(see the duplication finding). Tighten the `edit_file_multi` doc to say what is
actually guaranteed.
**Effort:** M

### [Minor][Contracts] `agent` tool description says "runs a single turn" but drives the full multi-iteration agent loop with no turn cap — `src/aj-tools/src/agent.rs:44`, `src/aj-agent/src/lib.rs:678,759`
**What:** The schema text the model sees states: "It runs a single turn and
returns a detailed report" (`agent.rs:44`). The implementation calls
`ctx.spawn_agent(...)` → `Agent::run_single_turn` → `execute_turn`, and
`execute_turn` is a full agentic loop (`'outer: loop` at `lib.rs:759`) that runs
inference, executes whatever tool batches the sub-agent requests, and repeats
until the sub-agent stops requesting tools. There is no iteration/turn cap on
that loop — the sub-agent is bounded only by cancellation and by the `agent`-tool
filter that prevents it from spawning further sub-agents (`lib.rs:1652`). So the
sub-agent can perform many model calls and arbitrarily many file edits / bash
commands; "single turn" describes the *API entry point* (one prompt, one report)
but materially misleads about cost and side-effect scope.
**Why it matters:** The description is the contract the model reasons about when
deciding whether to delegate. "Single turn" implies a cheap, bounded, read-only-ish
probe; the reality is an unbounded sub-agent with the full mutating toolset
(minus `agent`). A model that under-estimates the blast radius will over-delegate
risky work. The absence of any turn cap is also a runaway-cost footgun worth a
deliberate decision.
**Suggested action:** Reword the description to reflect that the sub-agent runs to
completion (multiple internal steps) and has read *and write* file tools plus
bash. Separately, decide with the user whether `run_single_turn`'s loop should
carry a turn/iteration ceiling so a confused sub-agent can't loop without bound.
**Effort:** S

### [Minor][Contracts] `edit_file` / `edit_file_multi` descriptions omit that matching is exact-substring with non-overlapping count semantics — `src/aj-tools/src/edit_file.rs:36-38`, `src/aj-tools/src/edit_file_multi.rs:44-45`, `src/aj-tools/src/edit_file.rs:110,132`
**What:** The descriptions say `old_string` "must match exactly one occurrence"
and that `replace_all` replaces "all occurrences," but never state that matching
is *literal substring* matching over the whole file (not line- or token-aware) and
that the count uses `str::matches`, which is **non-overlapping**. So an
`old_string` of `"aa"` against `"aaa"` counts as one match, not two, and a
single-occurrence guard can pass on input the model thought was ambiguous. Also
unstated: `replace_all` performs `String::replace`, which likewise replaces
non-overlapping left-to-right. The code comments note the non-overlapping choice
(`edit_file.rs:107-109`) but the model-facing description does not.
**Why it matters:** The exact-match/replace_all contract is the entire safety
model of these tools (refuse ambiguous edits). A model that doesn't know matching
is whole-file literal substring with non-overlapping counting can construct an
`old_string` that silently matches in a place it didn't intend, or mis-predict
the match count. Edge semantics that drive a safety guard belong in the contract.
**Suggested action:** Add a line to both descriptions: matching is exact literal
substring over the whole file, occurrences are counted non-overlapping, and
`replace_all` substitutes every non-overlapping occurrence.
**Effort:** S

### [Minor][Simplicity] `display_relative` and `error_outcome` are duplicated verbatim across four/five tool files — `src/aj-tools/src/read_file.rs:277,285`, `src/aj-tools/src/write_file.rs:122,132`, `src/aj-tools/src/edit_file.rs:162,172`, `src/aj-tools/src/edit_file_multi.rs:198,209`
**What:** `display_relative(path, cwd)` is byte-for-byte identical in `read_file`,
`write_file`, `edit_file`, and `edit_file_multi`. `error_outcome(path, error) ->
ToolOutcome` building a `ToolDetails::Text` error is identical in `write_file`,
`edit_file`, and `edit_file_multi` (and near-identical in `read_file`). The
absolute-path guard (`if !path.is_absolute() { error_outcome("Path must be
absolute…") }`) and the file-not-found / read-error handling are also copy-pasted.
**Why it matters:** Simplicity / duplication dimension. Four independent copies of
a path-display and error-shaping helper drift easily (one tool's error wording or
path-stripping behavior changing without the others), and they are exactly the
kind of cross-cutting shared concern the framework layer (`aj-tools` proper) is the
natural home for. This compounds the atomic-write finding: the fix wants one shared
`write_atomic` *and* one shared `error_outcome` / `display_relative` so all the
mutating tools behave uniformly.
**Suggested action:** Promote `display_relative`, `error_outcome`, the
absolute-path guard, and an atomic write helper into a small shared module in
`aj-tools` (e.g. `tools/fs_common.rs` or the existing `tools.rs`), and have the
four tools call it.
**Effort:** M

### [Minor][Simplicity] The match-count → guard → replace logic is duplicated between `edit_file` and `edit_file_multi` — `src/aj-tools/src/edit_file.rs:106-132`, `src/aj-tools/src/edit_file_multi.rs:131-159`
**What:** `edit_file` validates a single replacement (count matches, reject 0,
reject >1 unless `replace_all`, then `String::replace`). `edit_file_multi` does
the identical thing in a loop over `edits`, with the only differences being the
`Edit #N:` prefix on the error strings and the in-memory accumulation. `edit_file`
is functionally `edit_file_multi` with a one-element edit list and slightly
different error phrasing.
**Why it matters:** Two implementations of the same core replacement contract can
diverge (e.g. a future fix to overlap semantics or whitespace handling applied to
one and not the other). The duplication theme the audit is tracking across sibling
tools. It also raises the question of whether `edit_file` needs to exist as a
separate tool at all, or could be a thin wrapper presenting a simpler schema over
the same engine.
**Suggested action:** Extract a `apply_replacement(content, old, new, replace_all)
-> Result<String, ReplaceError>` (and a count/validate helper) shared by both
tools; have `edit_file` call it once and `edit_file_multi` call it per edit. Decide
with the user whether `edit_file` stays a distinct tool or becomes a wrapper.
**Effort:** M

### [Minor][Errors] Spawn-failure / read-error swallowing loses the underlying cause on the write path — `src/aj-tools/src/write_file.rs:89-93`
**What:** `write_file` reads the prior content for the diff and maps **any** read
error other than `NotFound` to `None` (`Err(_) => None` at `write_file.rs:92`),
treating "couldn't read the existing file" the same as "file is new." If the file
exists but is unreadable (permissions, it's a directory, an I/O error), the tool
reports the action as `created` (because `file_existed` is false) and produces a
`Diff` with an empty `before`, then attempts the write. The genuine cause is
discarded and never surfaced; the model sees a misleading "created" even when it
overwrote/clobbered something.
**Why it matters:** Error-handling dimension: a swallowed error on a mutating tool
that then mislabels its own action. The comment says "the write below will surface
a real failure if the path is genuinely unusable," but many read-failure cases
(e.g. a non-UTF-8 existing file) still permit the write to succeed, so the
mislabel sticks and the diff is wrong (empty `before` against a file that had
content).
**Why it matters / Suggested action:** Distinguish "file absent" from "file
present but unreadable": keep `NotFound => None`, but on other read errors either
surface a recoverable error or at least set `file_existed = true` / mark the diff
as unknown-before so the action label and diff don't lie.
**Effort:** S

### [Minor][Testing] No test covers a multibyte / non-UTF-8 / `\r\n` payload through the edit and write tools — `src/aj-tools/src/edit_file.rs:183`, `src/aj-tools/src/write_file.rs:143`, `src/aj-tools/src/read_file.rs:134`
**What:** Every edit/write test uses pure-ASCII LF content. None exercises:
(a) an `old_string` containing multibyte UTF-8 (the substring match and
`String::replace` are byte-correct on `&str`, but there's no regression guard);
(b) CRLF line endings (`read_file` splits on `lines()`, which strips both `\n`
and `\r\n`, so a CRLF file round-tripped through read→edit would silently
normalize observed line content — untested); (c) a file that is not valid UTF-8
(`fs::read_to_string` fails, and `write_file`/`edit_file` route that to an error
outcome — but `write_file`'s `Err(_) => None` arm specifically *swallows* it; see
the errors finding). The happy-path-only theme on exactly the boundary that
matters for a text-mutation tool.
**Why it matters:** Multibyte/line-ending handling was called out in scope as a
correctness concern. The tools are probably correct for UTF-8/LF, but the
behavior on CRLF and non-UTF-8 inputs is unspecified and untested, and the
`write_file` read-error swallow means a non-UTF-8 existing file is mishandled
silently.
**Suggested action:** Add tests: an edit whose `old_string`/`new_string` contain
multibyte characters; a write/edit over a file containing `\r\n`; and a read/edit
attempt on a deliberately non-UTF-8 file asserting the error outcome (and pinning
the `write_file` behavior once the errors finding is resolved).
**Effort:** S

### [Minor][Contracts] Neither read nor the mutating tools resolve symlinks or canonicalize paths, and the absolute-path contract is the only path guard — `src/aj-tools/src/read_file.rs:106-112`, `src/aj-tools/src/write_file.rs:75-81`
**What:** The only path validation is `path.is_absolute()`. There is no
`canonicalize`, no symlink resolution, and no confinement to the working
directory. `..` components in an absolute path are passed straight to the OS, and
writing through a symlink follows it. This is consistent with the stated design
(bash has "no permissions checks or sandboxing," and these tools are deliberately
unconfined), so it is not a traversal *bug* per se — but it is an undocumented
contract: callers/readers might assume `display_relative` (which `strip_prefix`es
the cwd) implies some cwd-confinement, when it is purely cosmetic.
**Why it matters:** Contracts dimension. The path model (absolute, unconfined,
symlink-following, no `..` rejection) is a real security-relevant property of
tools the model drives, and it is enforced/assumed by convention with no doc.
Worth recording explicitly so a future reader doesn't mistake `display_relative`
for a sandbox.
**Suggested action:** Add a one-line contract note on each tool's module doc that
paths are absolute and otherwise unconfined (no traversal rejection, symlinks
followed), matching the explicit "no sandboxing" stance bash already documents.
No behavior change unless the user wants confinement.
**Effort:** S

### [Minor][Simplicity] Dead per-snapshot throttle interacts with a no-op `emit_update` on `DummyToolContext`; the live-loop wake-up tick duplicates the debounce constant — `src/aj-tools/src/bash.rs:216-240`
**What:** The bash loop self-throttles `emit_update` to one snapshot per
`UPDATE_DEBOUNCE` (`bash.rs:218-222`) and *also* arms a `tokio::time::sleep`
arm of the same `UPDATE_DEBOUNCE` (`bash.rs:239`) purely to wake the loop so the
throttled emit fires for a quiet long-running command. This is correct, but per
the AG2 note: when the consumer's `emit_update` is a no-op (the production
`SessionContextWrapper` path forwards to the bus, but `DummyToolContext` and any
context that discards updates make the whole snapshot construction —
`snapshot_partial` clones both tail buffers, `bash.rs:573-574`) the throttling
governs work whose only output is dropped. The debounce constant is encoded
twice (the `>=` check and the sleep duration) so they can drift.
**Why it matters:** Simplicity / dead-work dimension and the AG2 dead-throttling
theme. `snapshot_partial` clones both rolling tails (up to `ROLLING_CAP_BYTES`
each) on every tick; when nobody consumes updates this is pure waste, and the
duplicated constant is an invariant-by-convention. Low severity because the
production path does consume updates and the cost is bounded.
**Suggested action:** Drive the loop wake-up off the same `UPDATE_DEBOUNCE`
interval via a single `tokio::time::Interval` (one source of truth for the
cadence), and consider skipping `snapshot_partial` construction when the context
advertises no update sink (would need a cheap `ToolContext` capability hint).
**Effort:** S

### [Nit][Comments] Module docs narrate migration chronology (`Migrated to … per docs/aj-next-plan.md §2.2`) — every tool file — `src/aj-tools/src/bash.rs:3`, `src/aj-tools/src/read_file.rs:1-2`, `src/aj-tools/src/agent.rs:4`, `src/aj-tools/src/write_file.rs:3-4`, `src/aj-tools/src/edit_file.rs:3`, `src/aj-tools/src/edit_file_multi.rs:4`, `src/aj-tools/src/todo.rs:3`
**What:** Every tool's module doc opens with "Migrated to
[`aj_agent::tool::ToolDefinition`] per `docs/aj-next-plan.md` §2.2." `read_file`
goes further: "first tool migrated to the new … surface." `todo.rs:34-36` says
"once the bus drives rendering (§2.3+), this helper stays here." These are
chronology / change-history framing the guidance asks comments to avoid — they
narrate a past migration and reference plan-doc sections rather than describing
the steady-state contract.
**Why it matters:** Comments-and-docs dimension; matches the AG1/AG2/TO1 finding
that plan-section references and "today/will" framing pervade the workspace. The
"first tool migrated" line is purely historical and will read as noise once the
migration is ancient.
**Suggested action:** Restate each module doc as the steady-state contract (what
the tool returns, its `ToolDetails` shape, its error model) and drop the
"Migrated … per §2.2" / "first tool" / "once §2.3+" framing.
**Effort:** S

### [Nit][Contracts] `read_file`'s `first_line_exceeds_limit` escape embeds the raw `input.path` unquoted into a shell command it tells the model to run — `src/aj-tools/src/read_file.rs:181-187`
**What:** The byte-overflow escape builds `sed -n '{N}p' {path} | head -c {bytes}`
with `input.path` interpolated unquoted. A path containing shell metacharacters
or spaces would produce a command the model can't run verbatim (and, if blindly
forwarded to bash, could behave unexpectedly). It's the model that would run it,
and the model is expected to adapt, so this is not an injection from the tool into
its own process — but the suggested command is not robust for unusual paths.
**Why it matters:** Minor contract/robustness: the escape is meant to be directly
actionable, and an unquoted path defeats that for paths with spaces or
metacharacters. Not a security issue here (the tool doesn't execute it), so a nit.
**Suggested action:** Shell-quote the path in the suggested command (e.g. wrap in
single quotes with escaping, or note it should be quoted).
**Effort:** S

## What's good

- **Recoverable-error discipline is uniform and correct.** Every tool turns
  input-validation and IO failures into `is_error: true` outcomes carrying a
  human-readable body, rather than bubbling `Err` and aborting the turn. The two
  deliberate exceptions are principled: `bash` only flags cancel/timeout as
  `is_error` (a non-zero exit is normal failure conveyed on the wire), and the
  `agent` tool *does* bubble spawn failures so the runtime can synthesize its
  standard error tool_result — both documented and tested.
- **`bash.rs` process management is careful and well-documented.** Fresh process
  group + `kill -9 -- -<pgid>` to reap grandchildren, `stdin: null` to prevent
  hangs on stdin-reading commands, a `biased` `select!` with cancel/timeout/exit
  arms, zombie reaping after kill, and a rolling-tail + spill-file design that
  bounds memory while preserving the full transcript on disk. The non-obvious
  decisions (process-group teardown, leading-edge debounce, the spill-keep-on-
  truncation policy) are all called out in the module doc.
- **`edit_file_multi` validates the whole batch in memory before any disk write**,
  so a mid-batch failure leaves the file untouched — and there's a direct test
  for it (`validation_failure_mid_batch_leaves_file_untouched`). (The remaining
  gap is that the *final* write itself is non-atomic; see the Major finding.)
- **Sub-agent recursion is bounded by design.** `agent.rs` delegates to
  `ToolContext::spawn_agent`, and the runtime filters the `agent` tool out of the
  sub-agent's inherited toolset (`aj-agent/src/lib.rs:1652`), so a sub-agent can't
  spawn further sub-agents — infinite recursion is structurally impossible. The
  tool returns a clean structured `SubAgentReport { agent_id, task, report }` and
  mirrors the report onto the wire.
- **`todo.rs` is a clean pair of tools.** State lives entirely in the session via
  `ToolContext::get/set_todo_list` (no hidden global), the one invariant (≤1
  in-progress) is validated as a recoverable error, content is sanitized before
  styling, and the structured `Todos` payload + text projection are both produced.
  The re-exported `aj-agent` todo types keep one canonical definition.
- **Error/edge-path test coverage is the best in the audit so far.** Truncation,
  partial-line, cancellation, timeout, missing-binary, working-dir, spill-file,
  zero/multi-match, missing-file, relative-path, and atomic-mid-batch cases are
  all exercised with readable fixtures and no wall-clock/network/ordering coupling
  beyond the deliberately bounded cancellation/timeout timing assertions.

## Boundary & architecture notes

Dependency direction is correct: the tools depend on `aj-agent` (the
`ToolDefinition` trait, `ToolContext`, `ToolDetails`, `ExecutionMode`,
`BashStreamTruncation`, todo types) and `aj-models` (`UserContent`), plus the
in-crate `truncate`/`sanitize`/`image` support modules. Nothing reaches up into
the binary or TUI. The `anyhow::Result` return type on every `execute` is
inherited from the `aj-agent` `ToolDefinition` trait (the AG2 anyhow-in-lib
decision), not re-introduced here — the impls almost never actually return `Err`
(they prefer recoverable `is_error` outcomes), so the trait's `anyhow` is mostly
unused surface at this layer. Public surface is reasonable: `format_todo_list`,
`format_for_display`, and `stream_marker` are `pub` for the renderer/bridge;
worth a synthesis check on whether all three still need to cross the crate
boundary.

## Test assessment

Tests exercise the public `execute` contract (input → outcome), not internals,
and cover the error and boundary paths thoroughly rather than just the happy
path. Fixtures use real temp files and (for images) synthesized PNGs; the bash
`RecordingCtx` wrapper around `DummyToolContext` is a clean way to assert
`emit_update` behavior. Notable gaps: (1) no multibyte / CRLF / non-UTF-8 payload
through the edit/write/read tools (see Testing finding) — the one correctness
dimension explicitly in scope that lacks a guard; (2) the non-atomic write failure
mode is untested (hard to test without fault injection, but the *contract* could
be); (3) `write_file`'s `Err(_) => None` read-swallow branch (existing-but-
unreadable file) is untested and currently mislabels the action. Timing-based
tests (cancellation/timeout) assert generous upper bounds (`< 5s`) so flakiness
risk is low.

## Cross-cutting themes to bubble up

- **Non-atomic file writes (CONFIRMED, new and highest-frequency locus).**
  `write_file`/`edit_file`/`edit_file_multi` all truncate-in-place via `fs::write`,
  the same data-loss theme as `aj-conf` (C1) and `aj-session` (SE1) but on the
  model's primary mutation surface, and `edit_file_multi` advertises an
  "atomic" contract its write does not honor. Synthesis should propose one
  workspace-wide atomic-write helper (temp + fsync + rename).
- **Contract/description mismatch (CONFIRMED).** The `agent` tool's "single turn"
  claim and the edit tools' silence on exact-substring/non-overlapping matching
  are model-facing contract gaps. Worth a synthesis pass over all tool
  descriptions for accuracy.
- **Duplication across sibling tools (CONFIRMED).** `display_relative`,
  `error_outcome`, the absolute-path guard, and the count→guard→replace logic are
  copy-pasted across four/five files — the same duplication theme TO1 noted for the
  disabled-tools filter. A shared `aj-tools` fs/error helper module is the natural
  home.
- **anyhow in lib crates (CONFIRMED, inherited not re-introduced).** Every
  `execute` returns `anyhow::Result` per the AG2 trait contract; the impls rarely
  exercise the `Err` arm. Fold into the workspace anyhow decision.
- **Dead/duplicated throttling for a no-op `emit_update` (CONFIRMED, minor).**
  bash's snapshot throttle + duplicated wake-up interval governs work whose output
  is discarded for non-consuming contexts — the AG2 dead-throttling theme.
- **Chronology / plan-section references in comments (CONFIRMED).** "Migrated …
  per §2.2", "first tool migrated", "once §2.3+" framing recurs in every tool
  module doc — the same comment-hygiene theme as AG1/AG2/TO1.
- **What held up clean (worth replicating):** the uniform recoverable-error model,
  bash's documented process-group teardown, `edit_file_multi`'s in-memory
  validate-before-write, structurally-bounded sub-agent recursion, and the
  error/edge-path-first test suites.
