# Audit findings — aj-session

- **Step:** SE1
- **Date:** 2026-06-02
- **Audited commit:** a477dca
- **Scope:** `src/aj-session/src/lib.rs`, `src/aj-session/src/log.rs`,
  `src/aj-session/src/persistence.rs`, `src/aj-session/src/listener.rs`,
  `src/aj-session/src/replay.rs`, `src/aj-session/src/repair.rs`,
  `src/aj-session/Cargo.toml` (incl. all in-module `#[cfg(test)]` suites).

## Summary

`aj-session` is a well-structured, well-documented crate, and the
fundamental durability design is sound: the on-disk format is **append-only
JSONL**, so writes extend the file rather than truncate-in-place. That side-steps
the C1 config-corruption class of failure by construction — there is no
write-temp+rename gap because there is no rewrite. The module boundaries are clean
(`log` owns the in-memory image + append API, `persistence` owns directory
discovery, `replay` projects to events, `repair` heals interrupted turns), the
forward/backward-compat story for the format is deliberate (`#[serde(default)]`,
`skip_serializing_if`, legacy-format detection), and the test suites are unusually
good — they exercise resume round-trips, the buffered-system-prompt sequencing,
the usage-accumulator ordering, and the legacy-payload fallbacks.

The findings cluster on three durability/contract themes the audit flagged.
First, **the per-line durability contract is overstated**: the docs say an entry
is "durable as soon as the call returns," but no `fsync`/`sync_data` is ever
issued and the `File` write buffer is not flushed, so a power loss can lose lines
the API reported as committed — and `repair_interrupted_tool_uses` depends on
exactly that per-line durability. Second, **nothing guards against two processes
writing the same session file**: `resume` opens in append mode with no lock, so
two `aj continue <id>` invocations interleave lines and mint colliding entry ids.
Third, **replay is lossy on timestamps** — the persisted `ConversationEntry::timestamp`
is dropped during projection, so resumed turns can't reconstruct their original
wall-clock time. The `anyhow`-in-a-lib-crate theme recurs (`repair.rs`, `listener.rs`).
None rise to silent-corruption-on-replay, but the durability-claim and
concurrency findings are Major because they touch the crate's central promise.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 3 | 5 | 3 |

## Findings

### [Major][Contracts] Per-line durability is claimed but never `fsync`'d — a crash can lose lines the API reported as committed — `src/aj-session/src/log.rs:461-532`, `src/aj-session/src/log.rs:11-13`
**What:** `append` writes punctuation entries with `writeln!(file, ...)` and
returns `Ok(id)` (`log.rs:521-524`). The module/type docs state the durability
contract in strong terms: "every individual event is durable as soon as the call
returns" (`log.rs:12-13`, `log.rs:694-695`) and "After `Ok(_)`, the entry and
everything that preceded it are durable" (`log.rs:444-447`). But there is **no
`flush()`, `sync_data()`, or `sync_all()` anywhere in the crate** (verified by
`rg sync_all|sync_data|flush` — only doc/test hits). `writeln!` on a `std::fs::File`
goes through the OS page cache; on power loss or hard kill the just-written line
(and possibly drained `pending_writes` before it) can be lost even though `append`
returned `Ok`. The class of crash that *is* tolerated — a truncated trailing line
on `resume` (`log.rs:387-403`) — is the same-process-died case, not the
power-loss case the "durable" wording implies.
**Why it matters:** This is the crate's central promise and the basis for
`repair_interrupted_tool_uses`, which the doc explicitly cites
(`log.rs:446-447`): repair only works if the assistant `tool_call` line actually
reached stable storage. An overstated durability contract is a data-loss footgun —
a caller (the persistence listener returning `Err` to abort the turn,
`listener.rs:14-17`) believes a successful append means the message survives a
crash, but it may not. The append-only design is the right foundation (no
truncate-in-place, unlike C1's `Config::save`); the gap is the missing sync, not
the write strategy.
**Suggested action:** Decide the intended durability level with the user.
Either (a) call `file.sync_data()` after the punctuation `writeln!` so the
contract is true (at a per-turn fsync cost, acceptable on the agent loop's
cadence), or (b) soften the docs to state writes are *buffered to the OS* and
survive a process crash but not necessarily a power loss, and that `fsync` is
deliberately omitted for throughput. Pair with a note on what `repair` can and
cannot recover.
**Effort:** S

### [Major][Contracts] No file lock or single-writer guard — two processes resuming the same session interleave lines and mint colliding entry ids — `src/aj-session/src/log.rs:368-430`, `src/aj-session/src/log.rs:551-559`
**What:** `resume` opens the backing file with `OpenOptions::new().append(true)`
(`log.rs:418`) and takes **no advisory lock**. Entry ids are a per-process
in-memory counter rendered as `{:08}` (`log.rs:555-558`), seeded on resume from
the max id already on disk (`log.rs:409-413`). If a user runs `aj continue <id>`
twice (two terminals) against the same project, both processes load the same
`next_counter`, both mint id `00000042`, and both append to the file. The result
is interleaved lines, **duplicate ids**, and a parent chain that no longer
linearizes deterministically — `entries.insert(id, ...)` (`log.rs:530`) silently
overwrites on the duplicate, and `linearize`/`latest_leaf` walk a corrupted
structure. The doc acknowledges single-writer assumptions only obliquely
("single-writer setup", `log.rs:353-354`; "the listener has exclusive write
access", `listener.rs:29`) but nothing enforces it.
**Why it matters:** This is the audit's explicit concurrency cross-check. The
"exclusive write access" the listener doc relies on is a *within-process*
invariant (one listener owns the `Arc<TokioMutex<ConversationLog>>`); it says
nothing about a second process. `aj continue` and `aj continue <id>` are
first-class CLI verbs, so opening the same session twice is a plausible user
action, and the failure is silent on-disk corruption that only manifests on the
next resume. This is precisely the "silent data loss / corruption" class the audit
treats as at least Major.
**Suggested action:** Take an OS advisory lock (`flock`/`fcntl` via a small
dependency, or a sidecar `.lock` file created with `create_new`) in `resume`/the
lazy `ensure_open`, releasing on drop, and surface a clear "session already open
in another process" error. At minimum, document the single-writer-per-file
invariant on `ConversationLog` as a hard precondition with the corruption
consequence spelled out.
**Effort:** M

### [Major][Misc] Replay drops the persisted entry timestamp — resumed turns lose their original wall-clock time — `src/aj-session/src/replay.rs:100-281`, `src/aj-session/src/log.rs:84-85`
**What:** Every `ConversationEntry` persists a `timestamp: Option<DateTime<Utc>>`
(`log.rs:84-85`, set to `Utc::now()` at append, `log.rs:504`). `replay`'s
projection (`project_entry`, `project_assistant`, `project_tool_result`) builds
`MessageStart`/`MessageEnd`/`ToolExecution*`/`TurnUsage` events but **never reads
`entry.timestamp`**. The synthesized events carry whatever timestamp the
re-projected `AgentMessage` happens to hold (for the empty `MessageStart`
placeholder, `assistant.timestamp` is copied at `replay.rs:169`, but the framing
`ConversationEntry::timestamp` — the authoritative append time — is discarded).
On resume, a frontend rendering "sent at HH:MM" or computing turn durations sees
either `None` or the message's own (possibly absent) timestamp, not the recorded
append instant.
**Why it matters:** This is the audit's "replay must be lossless and reconstruct
UI state faithfully" cross-check, and it's the wall-clock-nondeterminism theme:
the log *records* the real time but replay throws it away, so the resumed UI can't
reproduce the original timeline. It's a Major (not Critical) because the message
content itself round-trips faithfully — the loss is metadata, not conversation —
but it is a genuine fidelity gap between live and resumed sessions, which is the
stated design goal ("Resuming a session should look the same to a frontend as a
live run", `replay.rs:5-8`).
**Suggested action:** Thread `entry.timestamp` through projection and stamp it
onto the synthesized events (or onto the `AgentMessage`/`AssistantMessage` if the
event types don't carry a framing timestamp). If `AgentEvent` has no timestamp
slot, flag for synthesis whether the event bus should carry one. Add a replay test
asserting the projected event's time matches the persisted entry time.
**Effort:** M

### [Minor][Errors] `repair_interrupted_tool_uses` and the persistence listener return `anyhow::Error` from a library crate — `src/aj-session/src/repair.rs:42-45`, `src/aj-session/src/listener.rs:39,104-138`
**What:** `repair_interrupted_tool_uses` returns `Result<bool, anyhow::Error>`
(`repair.rs:45`), and `listener.rs` uses `anyhow::{Result, anyhow}` throughout
(`listener.rs:39`, `persist` returning `anyhow::Result<()>`, `anyhow!(...)` errors
at `listener.rs:71,76,128`). `aj-session` is a library crate and otherwise defines
a proper `thiserror` `ConversationError` (`log.rs:33-43`) that already has an
`InvalidAppend`/`Io`/`Json`/`Corrupt` taxonomy. The `anyhow` usage is the
recurring workspace theme (AG2 flagged it in the tool contract, M1 in `refresh.rs`,
C1 nearby). Note the listener edge is partly forced: the bus `Listener` signature
likely returns `anyhow::Result`, so `listener.rs` is adopting an upstream choice,
whereas `repair.rs` is a free function that could return `ConversationError`.
**Why it matters:** Library error types should be `thiserror` so callers can match
on failure modes; an `anyhow::Error` from `repair` collapses "log append failed"
(`view.add_message?` at `repair.rs:91`) and any future logical error into one
opaque string. Consistency within the crate matters too — `repair` is the only
public API not speaking `ConversationError`.
**Suggested action:** Change `repair_interrupted_tool_uses` to return
`Result<bool, ConversationError>` (its only fallible call is `add_message`, which
already yields `ConversationError`). Fold the listener's `anyhow` use into the
workspace-wide anyhow decision, since it tracks the bus `Listener` trait's error
type rather than being a free choice here.
**Effort:** S

### [Minor][Contracts] A truncated/partial assistant turn is persisted and replayed as if complete — only dangling tool_calls are healed — `src/aj-session/src/repair.rs:42-94`, `src/aj-session/src/log.rs:461-532`
**What:** The crate's only crash-consistency mechanism is
`repair_interrupted_tool_uses`, which heals exactly one shape: an assistant
`tool_call` with no matching `tool_result` (`repair.rs:27-31`). Any *other*
partial turn is silently accepted as complete. Because each `MessageEnd` is its own
JSONL line, a process killed mid-turn after persisting an assistant message that
contains, say, partial/streamed text but whose `stop_reason` indicates it was cut
off, leaves a fully-parseable line; `resume` keeps it (`log.rs:387-416`), `replay`
emits it as a finalized `MessageEnd` (`replay.rs:175-178`), and nothing flags it as
truncated. The "truncated trailing line" tolerance (`log.rs:390-403`) only catches
a *byte-truncated* final line that fails to parse — not a *semantically*
incomplete-but-well-formed entry.
**Why it matters:** This is the audit's "truncation-looks-like-success"
cross-check. The persisted-message granularity means most interrupted turns
*are* recoverable (the dangling-tool_call case is the common one and is handled),
so this is Minor rather than Major — but the contract should state that the
durability unit is the finalized `MessageEnd`, so a turn aborted *before*
`MessageEnd` is simply absent (good) while a turn whose `MessageEnd` was written
with truncated content is replayed verbatim (a hole). No code change may be needed;
the gap is an unstated assumption.
**Suggested action:** Document on `append`/`repair` that the crash-consistency
unit is the finalized message line and that only dangling tool_calls are repaired;
note explicitly that a persisted-but-content-truncated assistant message is treated
as authoritative. If finer recovery is wanted, that's a larger design discussion to
have with the user.
**Effort:** S

### [Minor][Simplicity] `looks_like_new_format` re-opens and re-parses each session file's first line, duplicating the preview walk — `src/aj-session/src/persistence.rs:71-127,200-224,344-399`
**What:** `list_sessions` and `preview_candidates` both call
`looks_like_new_format` (`persistence.rs:71`, `:222`), which opens the file and
parses its first non-empty line as a `ConversationEntry` to filter pre-refactor
files. For previews, `read_session_preview_file` then re-opens the same file and
re-parses *every* line (`persistence.rs:355-399`) — so each previewed session is
opened twice and its first line parsed twice. With many sessions this is a
redundant `open`+parse per file on the selector's hot path.
**Why it matters:** `SessionPreview` is explicitly O(file size) per session
(`persistence.rs:251-257`) and drives a "Loading X/Y" indicator, so it's the one
place users feel latency. The format check is cheap relative to the full walk, so
this is Minor, but the double-open is avoidable duplication.
**Suggested action:** Have the preview walk perform the format check inline (treat
a first-line parse failure as "skip this file") instead of a separate
`looks_like_new_format` pass, so each file is opened once. Keep the standalone
check for `list_sessions`, which doesn't otherwise read the file.
**Effort:** S

### [Minor][Boundaries] Broad `pub` surface on `Conversation`/`ConversationView` accessors with no in-crate callers visible — verify against the binary — `src/aj-session/src/log.rs:194-269,696-770`
**What:** `Conversation` exposes `from_entries`, `conversation_id`, `entries`,
`len`, `is_empty`, `message_count`, `messages`, `agent_messages`, `last_message`
all `pub` (`log.rs:194-269`); several are only exercised by tests within the crate
(`message_count`, `messages`, `last_message` appear in tests but not in other
`aj-session` modules). `EntryId` is a public `type` alias for `String`
(`log.rs:50`), so entry ids are stringly-typed across the crate's whole API. These
are consumed by the `aj` binary (out of scope here), so the surface may be
justified — but the audit should confirm each `pub` has a real binary caller rather
than being test-only or vestigial.
**Why it matters:** Minimal public surface is a rubric dimension; a stringly-typed
`EntryId` invites the id-collision class the concurrency finding describes (a
newtype wouldn't prevent it but would make ids harder to fabricate). This is a
note-for-synthesis rather than a defect: the real check is cross-crate.
**Suggested action:** In the X1 synthesis (with the binary's usage in hand),
demote any `Conversation`/`ConversationView` method with no binary caller to
`pub(crate)`, and consider a `EntryId(String)` newtype if synthesis confirms ids
flow through the binary untyped.
**Effort:** S

### [Minor][Contracts] `linearize` silently truncates the chain on a missing/broken parent pointer — `src/aj-session/src/log.rs:573-587`
**What:** `linearize` walks `parent_id` pointers and breaks the loop the moment
`self.entries.get(&id)` returns `None` (`log.rs:577-579`): "`let Some(entry) =
... else { break }`". A dangling parent id (which the concurrency/duplicate-id
finding can produce, or a hand-edited file) silently yields a *partial*
conversation — the root and everything above the break is dropped — with no
warning. The append-side validates that a parent exists (`log.rs:488-493`), so this
is unreachable in single-writer normal operation, but the read side degrades
silently when the invariant is violated.
**Why it matters:** A silently-shortened conversation handed to the model is a
correctness hazard (the model loses earlier context) that produces no log line. It
pairs with the concurrency finding: the two failure modes compound.
**Suggested action:** On a missing parent in `linearize`, emit a `tracing::warn!`
identifying the broken id (mirroring the `resume` truncated-line warning), so a
corrupted chain is at least observable. Document that `linearize` returns a partial
view rather than erroring on a broken chain.
**Effort:** S

### [Nit][Comments] Module docs reference `docs/aj-next-plan.md §N` and narrate "previous direct `view.add_*` calls" — chronology theme — `src/aj-session/src/lib.rs:19`, `src/aj-session/src/listener.rs:1-31`, `src/aj-session/src/replay.rs:10`, `src/aj-session/src/repair.rs:15`
**What:** Same recurring theme AG1/AG2/M1 flagged. `lib.rs:19` ends "See
`docs/aj-next-plan.md` §1, §2.0(a), and §2.5." `listener.rs` cites
"§2.4b", "§1.4", "§1.6", "§2.4b and §2.5" and narrates a migration ("The agent
(after §2.4b) never reaches into the log directly"; "preserving the same
durability guarantee the previous direct `view.add_*` calls had",
`listener.rs:15-17`). `replay.rs:10` and `repair.rs:15` also cite plan-doc
sections.
**Why it matters:** Comments should stand alone from the current code; coupling
them to external plan-doc section numbers and to "previous" implementations makes
them go stale and forces a reader to find a doc that may not match HEAD. The design
rationale is worth keeping; the §N pointers and the change-history framing are not.
**Suggested action:** Drop the `§N` references and the "previous direct calls"
chronology; restate the durability-on-`Err` rationale and the sub-agent anchoring
rule as steady-state design prose. Resolve alongside the workspace-wide chronology
cleanup.
**Effort:** S

### [Nit][Contracts] `mint_unique_path`'s 1000-collision ceiling and `next_id`'s `{:08}` 8-digit format are silent upper bounds — `src/aj-session/src/log.rs:346-359,555-558`
**What:** `mint_unique_path` retries collision suffixes `_1.._999` then errors
(`log.rs:346-359`) — fine, and the comment justifies it. Separately, `next_id`
formats the counter as `{:08}` (`log.rs:556`); past 100M entries in one log the
zero-padding stops aligning and lexicographic sort no longer matches append order,
which the `id` doc promises ("Monotonic so lexicographic sort matches append
order", `log.rs:75-76`). `parse_id_counter` parses the full `u64` so resume still
works past 8 digits, but the sort-order invariant quietly breaks. 100M entries per
session is implausible, so this is a Nit.
**Why it matters:** A documented invariant (lexicographic == append order) has an
undocumented breaking point. Worth a one-line nota bene rather than a fix.
**Suggested action:** Add a comment on `next_id` noting the `{:08}` padding
assumes < 100M entries per log and that the lexicographic-sort invariant holds only
within that bound (which it always will in practice).
**Effort:** S

### [Nit][Style] `read_session_preview_file` uses `match { _ => {} }` where `if let` reads cleaner — `src/aj-session/src/persistence.rs:380-398`
**What:** The per-line loop matches `&entry.entry { ConversationEntryKind::Message
{ .. } => {...}, _ => {} }` (`persistence.rs:380-398`) with a no-op catch-all. An
`if let ConversationEntryKind::Message { message: msg } = &entry.entry { ... }`
expresses the single-arm-of-interest intent more directly. Trivial.
**Why it matters:** Minor readability; the `_ => {}` arm is the kind of noise
clippy/style guidance discourages when only one variant is handled.
**Suggested action:** Replace with `if let`. Cosmetic.
**Effort:** S

## What's good

- **Append-only format dodges the C1 corruption class by construction
  (`log.rs:461-532`).** There is no rewrite-the-whole-file step, so the
  write-temp+rename discipline C1 needs for `config.toml` is unnecessary here: a
  crash mid-append can only truncate the *last* line, which `resume` explicitly
  tolerates (`log.rs:387-403`). The lazy file creation (`ensure_open` with
  `create_new(true)`, `log.rs:540-549`) plus the punctuation-vs-buffered
  distinction (`is_punctuation`, `log.rs:121-141`) is a genuinely thoughtful
  design that prevents accumulating empty session files — and it's tested from
  both directions (`set_system_prompt_alone_does_not_create_file`,
  `first_punctuation_append_flushes_buffered_system_prompt`).
- **Forward/backward schema-compat is deliberate and tested.** `#[serde(default)]`
  on `timestamp`/`agent_id`/`parent_id` (`log.rs:81-93`), `skip_serializing_if` to
  keep lines lean, the `Message`-nested-under-`message` decision to avoid a
  `timestamp` collision (`log.rs:104-110`), and the `looks_like_new_format`
  pre-refactor gate (`persistence.rs:108-127`) together let new code read old logs
  and skip incompatible ones gracefully. The legacy-shape preview test
  (`list_session_previews_falls_back_to_modified_when_no_message_entries`) pins a
  real on-disk shape older builds produced.
- **Replay ordering faithfully mirrors the live agent.** The
  `MessageStart`(empty placeholder)/`MessageEnd`(finalized) pairing
  (`replay.rs:154-178`), the per-turn `TurnUsage` with pre-add accumulator
  semantics (`replay.rs:180-207`), and the independent per-`AgentId` accumulators
  are all reconstructed in live-agent order and locked by precise tests
  (`replay_synthesizes_turn_usage_per_assistant_message`,
  `replay_keeps_main_and_subagent_usage_accumulators_separate`). The
  tool_call→tool_result labeling map and the `text_fallback` for absent/corrupt
  `details` (`replay.rs:228-313`) are correct and tested for the orphan case.
- **Persistence is a genuine pure subscriber (the boundary holds).** `listener.rs`
  builds a `Listener` closure over `Arc<TokioMutex<ConversationLog>>` and only ever
  reacts to `MessageEnd`/`SubAgentStart` (`listener.rs:54-93`); it never calls back
  into the agent runtime. The sub-agent first-entry anchoring via the
  `pending_anchors` map is the right place for that logic and is well-tested
  (`sub_agent_first_message_anchors_at_parent_head`,
  `sub_agent_assistant_without_anchor_returns_error`). Dependencies are exactly
  `aj-agent` + `aj-models` as the graph requires.
- **The `last_message_at` max-not-last logic (`persistence.rs:360-395`).** Tracking
  the largest message timestamp rather than the last line correctly tolerates
  out-of-order writes (a tool result finalizing after a streamed assistant), with
  the reasoning stated and tested (`list_session_previews_uses_largest_message_timestamp`).
- **`ConversationError` is a proper `thiserror` enum** with a useful taxonomy
  (`Io`/`Json`/`Corrupt`/`InvalidAppend`, `log.rs:33-43`); the `append` invariant
  checks surface as `InvalidAppend` errors rather than panics (`log.rs:468-498`),
  matching the "prefer errors over panics on agent-side bugs" comment.

## Boundary & architecture notes

Dependency direction matches `CLAUDE.md`: `aj-session` depends only on `aj-agent`
and `aj-models` (`Cargo.toml:7-8`), reaching *down* the graph for `AgentMessage`,
`AgentEvent`/`AgentId`, `ToolDetails`, `TokenUsage`, and the wire `Message` types —
never sideways or up into the TUI/binary. Persistence is genuinely "just another
subscriber" (the `persistence_listener` closure), confirming the CLAUDE.md claim
that the binary owns the log and registers a listener; nothing here couples back
into the runtime. One Listener-error edge (`listener.rs` `anyhow`) tracks the bus
trait's error type and should be resolved at the bus boundary, not here.

For synthesis: (1) the `pub` surface on `Conversation`/`ConversationView` and the
stringly-typed `EntryId` need the binary's usage to judge minimality (see Minor
finding); (2) the single-writer-per-file invariant is workspace-relevant — the
binary's `aj continue`/`aj continue <id>` paths should be checked for whether they
can open the same file twice.

## Test assessment

Tests are a strength and exercise the real boundaries, not internals: resume
round-trips (`add_message_tool_result_round_trips_through_resume`), the
buffered-system-prompt sequencing on disk, the usage-accumulator ordering and
isolation, the structured-`ToolDetails` resume path, the orphan-tool_result
fallback, and the dangling-tool_call repair. The legacy on-disk shapes are covered
(`list_session_previews_falls_back_to_modified_when_no_message_entries`,
`parse_session_id_*`). Fixtures are clean — per-test temp dirs keyed on
pid/thread/nanos avoid cross-test collisions.

Notable gaps, all matching findings above:
- **No durability/crash test.** Nothing asserts data survives (or the doc's claim
  about) a crash; given there is no `fsync`, a test that resumes after a simulated
  truncated trailing line *exists* (the `resume` warning path) but there is no test
  that a `flush`/`sync` happened — because none does. The durability contract is
  untested precisely where it's overstated.
- **No concurrency test.** No test opens the same session file from two
  `ConversationLog`s; the duplicate-id/interleave corruption is undetected.
- **Replay timestamp fidelity is untested** (and currently lossy).
- **`linearize` on a broken parent chain is untested** — the silent-truncation
  branch has no coverage.

No real-network coupling. The preview tests use small `sleep`s to separate
millisecond-resolution mints (`persistence.rs` tests at `:559,:577,:748`), which is
a mild wall-clock coupling but bounded and low flakiness risk.

## Cross-cutting themes to bubble up

- **Non-atomic / non-synced user-file writes (CONFIRMED, distinct sub-case).** C1
  flagged truncate-in-place for `config.toml`; `aj-session` *avoids* that with
  append-only writes, but introduces a different gap — **no `fsync`** — while
  documenting writes as "durable." Synthesis should treat the user-file durability
  sweep as two axes: (a) atomicity of rewrites (config/themes) and (b) fsync of
  appends (sessions).
- **anyhow in lib crates (CONFIRMED, recurs).** `repair.rs` returns
  `anyhow::Error`; `listener.rs` uses `anyhow` (the latter tracking the bus
  `Listener` trait). Fold into the one workspace anyhow decision.
- **Wall-clock non-determinism (CONFIRMED, new locus).** Replay drops the persisted
  entry timestamp, so resumed sessions can't reconstruct their original timeline —
  the log records real time but the projection discards it.
- **Truncation-looks-like-success (PARTIALLY held).** Byte-truncated trailing lines
  are tolerated and most interrupted turns are recoverable (dangling tool_calls are
  repaired), but a content-truncated-yet-well-formed `MessageEnd` is replayed as
  authoritative with no flag — an unstated assumption rather than a bug.
- **Concurrency / single-writer assumption (NEW).** Multiple processes can open the
  same session file with no lock, producing duplicate ids and interleaved lines.
  Worth a workspace-wide check of every on-disk-file owner for multi-process safety.
- **Stringly-typed ids across a persistence boundary (NEW, minor).** `EntryId =
  String` echoes AG2's `usize` sub-agent-id-by-convention note; synthesis may want
  a coherent id-typing pass across event/tool/session boundaries.
- **What held up clean:** the append-only format design, the persistence-as-pure-
  subscriber boundary, replay's live-order fidelity (modulo timestamps), and the
  deliberate forward/backward serde-compat are strong, replicable patterns.
