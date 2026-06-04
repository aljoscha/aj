# Audit findings — aj-core

- **Step:** A2
- **Date:** 2026-06-02
- **Audited commit:** 9d3e6ab
- **Scope:** `src/aj/src/main.rs`, `src/aj/src/lib.rs`,
  `src/aj/src/modes.rs`, `src/aj/src/modes/print.rs`,
  `src/aj/src/persistence.rs`, `src/aj/src/scripted.rs` (incl. the
  in-module `#[cfg(test)]` suite in `print.rs`). Cross-referenced
  `src/aj/src/modes/interactive.rs` (A3 scope) for the session-setup
  duplication, and `src/aj-session/src/listener.rs` for the persistence
  contract.

## Summary

The binary's entrypoint and print mode are well-documented and the
persistence wiring is genuinely clean: `persistence.rs` is an empty
doc-only module and the actual listener is `aj-session`'s
`persistence_listener`, registered as a pure bus subscriber with no
back-coupling into the runtime — confirming the `CLAUDE.md` "binary owns
the log, registers a listener" contract and the SE1/AG1 findings. Print
mode's error taxonomy (Recoverable/Aborted/Fatal → non-zero exit) and
SIGINT handling are thoughtfully designed and the `collect_prompt_text`
dispatch is the only thing with real unit coverage.

The findings cluster on two themes the audit predicted. First, the
**entire session-setup sequence is duplicated** between `print.rs::run`
and `interactive.rs::run` — log resume/create, system-prompt freeze,
sub-agent-counter seeding, the repair + re-linearize + seed walk, and
the model/tool/precedence assembly — generalizing A1's precedence-merge
and disabled-tools duplication into a missing "build the agent's startup
inputs" composition root. Second, `main.rs` loads `~/.aj/.env` **after**
initializing the logger, and the `AJ_LOG_FILE` open uses `?` so a bad
log path aborts the whole binary before any arg parsing. The
cross-process single-writer concern SE1 raised is unguarded here (two
`aj continue <id>` invocations open the same file with no lock), but the
binary opens each log exactly once per process, so it doesn't *add* a
double-open. Print mode does **not** rely on the vestigial
`AgentEnd.messages` field (AG1) — it walks `agent.messages()` directly.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 2 | 5 | 2 |

## Findings

### [Major][Simplicity] The entire session-setup sequence is duplicated between print and interactive mode with no composition root — `src/aj/src/modes/print.rs:110-357`, `src/aj/src/modes/interactive.rs:284-357`
**What:** `print::run` and `InteractiveMode::run` each independently
re-derive the same startup pipeline, step for step: build the tool list
+ filter disabled tools (`print.rs:143-149`, A1 already flagged the
filter copy), the scripted-vs-registry agent branch with the
precedence-merged model args (`print.rs:159-219`, A1's five-site
precedence finding), resume-or-create the `ConversationLog`
(`print.rs:230-242` vs `interactive.rs:294-310`, byte-for-byte the same
match shape modulo the "no latest" message), the system-prompt
reuse/freeze block (`print.rs:251-260` vs `interactive.rs:319-328`,
identical), the sub-agent counter seed (`print.rs:265-267` vs
`interactive.rs:332-334`, identical), and the repair + re-linearize +
`seed_messages` walk (`print.rs:286-297` vs `interactive.rs:346-357`,
identical including the `expect("post-repair head exists…")`). The only
real difference is which subscriber drives the bus — exactly the
single-difference `main.rs:67-69` documents. There is no shared
"assemble the agent, tools, log, and seeded transcript from `Args` +
`Config`" function; both modes inline ~120 lines of the same orchestration.
**Why it matters:** This is the binary-level "no composition seam" smell
A1 and TO1 predicted, now at its widest. Six logically-identical blocks
must stay in lockstep across two files; they have already begun to drift
(print logs the disabled-tools set, interactive doesn't; print bails on
"no latest session", interactive prints a warning and starts fresh). A
change to the resume handshake (e.g. SE1's proposed file lock, or
threading the persisted timestamp) must be made twice, and the repair
walk's `expect` is duplicated rather than centralized. The modes should
*share* setup and differ only in the subscriber + the loop.
**Suggested action:** Extract a `SessionSetup` (or
`fn build_session(args, config) -> (Agent, Arc<TokioMutex<ConversationLog>>, …)`)
that owns tool assembly, the model/precedence merge (folding A1's
proposed `ModelSelection`), log resume/create, system-prompt resolution,
counter seeding, and the repair+seed walk. Both modes call it, then
register their own listeners and drive the loop. Collapses two ~120-line
blocks to one and gives the startup pipeline a single home.
**Effort:** L

### [Major][Errors] `AJ_LOG_FILE` open failure and the logger-before-dotenv ordering can abort the binary before arg parsing — `src/aj/src/main.rs:22-46`
**What:** `main` initializes the tracing subscriber first
(`main.rs:22-35`); the `AJ_LOG_FILE` branch opens the file with
`OpenOptions…open(&path)?` (`main.rs:28`) and propagates the error out of
`main`, so a non-writable `AJ_LOG_FILE` (typo, read-only dir, parent
missing) terminates the whole binary with an opaque I/O error *before*
`Args::parse()` or any command runs — including commands like
`list-sessions` that don't need logging at all. Separately, the logger is
initialized **before** the `.env` files are loaded (`main.rs:37-46`), so
`RUST_LOG`/`AJ_LOG_FILE` set in `~/.aj/.env` or the project `.env` are
*not* honored — only the ambient process env is. `EnvFilter::from_default_env`
reads `RUST_LOG` at `main.rs:22`, before `dotenv::from_path` runs at
`main.rs:42`. The `.env` precedence comment (`main.rs:37-39`) is correct
for *config* env vars (clap reads them at `Args::parse()`, after dotenv),
but logging silently sits outside that contract.
**Why it matters:** A diagnostics knob that aborts the program it's meant
to instrument is a footgun, and the failure mode (whole binary dies on a
bad log path) is disproportionate. The dotenv-after-logger ordering means
`AJ_LOG_FILE`/`RUST_LOG` in `~/.aj/.env` — the documented place for
env-driven config — quietly don't apply to logging, an inconsistency a
user will hit when they try to capture logs via their env file. The two
issues compound: there's no clean place to recover because the order is
backwards.
**Suggested action:** Load the `.env` files *before* initializing the
subscriber so `RUST_LOG`/`AJ_LOG_FILE` from `~/.aj/.env` are honored
(matching the documented env precedence). Treat a failed `AJ_LOG_FILE`
open as non-fatal: log a warning to stderr and fall back to stderr
logging rather than `?`-ing out of `main`. Decide with the user whether
`.env`-driven `RUST_LOG` is intended (it is the natural reading of the
"`.env` first" contract).
**Effort:** S

### [Minor][Contracts] Cross-process single-writer (SE1) is unguarded at the binary, where the multi-process risk actually originates — `src/aj/src/main.rs:50-63`, `src/aj/src/modes/print.rs:230-242`
**What:** SE1 flagged that `aj-session` takes no file lock, so two
processes resuming the same session interleave lines and mint colliding
ids, and asked synthesis to check "whether the binary can open the same
file twice." Within a single process it cannot: print mode opens the log
exactly once (`print.rs:230`) and interactive once (`interactive.rs:294`).
But the binary is the layer that *spawns* the second process: `aj
continue <id>` and `aj --print continue <id>` are first-class verbs
(`main.rs:53-61`, `print.rs:102-108`) that a user can plausibly run twice
concurrently (two terminals, or a script plus an interactive session),
and nothing in the binary detects or warns about an already-open session.
The binary owns the lifecycle decision (which session to open, when) but
delegates the safety question entirely to `aj-session`, which also
doesn't enforce it.
**Why it matters:** This is the consumer end of SE1's Major concurrency
finding. The corruption (duplicate `{:08}` ids, interleaved lines,
non-linearizable parent chain) is silent and only manifests on the *next*
resume. Because the binary is where the resumable session id is resolved
(latest-for-project or explicit), it's a natural place to acquire a lock
or surface "session already open" — and the only place that knows it's a
fresh process rather than an in-process re-open.
**Suggested action:** Coordinate with SE1's fix: if `aj-session` grows an
advisory lock in `resume`, the binary just surfaces its error cleanly
(both modes already wrap resume in `.with_context`). If the lock lands at
the binary instead, acquire it next to the `ConversationLog::resume` call
in both modes (which the shared `SessionSetup` from the Major finding
would centralize). At minimum, document the single-writer-per-session
precondition on the `continue` verb.
**Effort:** M

### [Minor][Errors] A stdout write failure in JSON mode is logged-and-dropped, so a broken pipe silently loses events mid-stream — `src/aj/src/modes/print.rs:437-452`
**What:** `json_event_listener` writes each event with `writeln!(io::stdout(), …)`
and, on a write error, prints a one-line warning to stderr and
*continues* (`print.rs:441-443`). The module doc justifies this for the
*serialization*-failure case (the `#[serde(skip)]` deferred variants,
`print.rs:430-436`) — sound, since skipping an unserializable event keeps
the run alive. But the same arm also swallows a genuine *stdout* write
failure (e.g. the consumer closed the pipe), and there the right behavior
is the opposite: the consumer is gone, so the agent should stop, not keep
burning tokens emitting events nobody reads. The run continues to
completion, persistence keeps writing, and the user sees a stderr warning
per event but no failure exit. This is the inverse of the persistence
listener's "an `Err` aborts the turn" durability contract
(`listener.rs:14-17`) — here the JSONL sink is the user's only output and
a write failure is treated as recoverable noise.
**Why it matters:** A broken-pipe on the primary output channel should be
a clean shutdown, not a silently-continuing run. The deliberate
"don't-fail-on-skipped-variant" rationale is correct but is applied too
broadly: it conflates "this one event can't be serialized" (skip it) with
"stdout is gone" (stop). The comment block (`print.rs:311-315`) even
*claims* the JSONL listener is registered before persistence "so that
when persistence errors out … the user has already seen every event up to
the failure on stdout" — but if stdout itself errors, that ordering
guarantee is hollow because the write was dropped.
**Suggested action:** Distinguish the two failure classes: on a
serialization error, log + skip (current behavior); on a stdout write
error (`writeln!` returning `Err`), return the error from the listener so
the bus aborts the run with `TurnError::Fatal` — matching the
persistence-listener durability contract and stopping a run whose consumer
has disappeared.
**Effort:** S

### [Minor][Contracts] `@file` expansion is wired into print mode but is a no-op stub, and only print mode calls it — `src/aj/src/modes/print.rs:412-422`
**What:** `collect_prompt_text` runs the joined prompt through
`file_args::expand` (`print.rs:421`), documented in the function and
module docs as `@file` expansion ("run them through `@file` expansion",
`print.rs:393-395`; the resume doc references it too). But
`file_args::expand` is `Ok(prompt)` verbatim (A1's Major finding,
`file_args.rs:23`), so the call does nothing, and A1 confirmed the
interactive entry point never calls `expand` at all. So the only live
call site of the stub is here, and it's advertised as a working feature.
**Why it matters:** Confirms A1's "contract advertised but not
implemented, wired into only one mode" finding from the print-mode side: a
user running `aj --print "summarize @src/main.rs"` gets the literal token
sent to the model. The `print.rs` doc compounds A1 by describing the
passthrough as if it expands. This is tracked as A1's Major; noted here so
the print-mode docs are corrected alongside whatever decision is made
there.
**Suggested action:** Fold into A1's `expand` decision: either implement
the resolver and call it from both modes (via the shared `SessionSetup`),
or strip the `@file` claim from `print.rs`'s module and
`collect_prompt_text` docs. Don't leave a no-op call advertised as a
feature in only one mode.
**Effort:** S

### [Minor][Simplicity] The text-mode no-op stream listener exists only to keep the bus non-idle and is dead weight — `src/aj/src/modes/print.rs:329-332`
**What:** In JSON mode a JSONL listener is subscribed; in text mode no
listener is subscribed (`_stream_handle = None`). The comment block
(`print.rs:322-328`) describes a text-mode "synchronous beat per event so
the bus is not idle (debug ergonomics)" listener "essentially a no-op …
keeps the structure symmetrical" — but the code path actually registers
*nothing* for text mode (`PrintFormat::Text => None`). So the comment
describes a listener that isn't there: either it was removed and the
comment is stale chronology, or it was never added. Text mode's rendering
happens entirely after `prompt` returns via `print_final_assistant_text`,
so no per-event listener is needed at all.
**Why it matters:** The comment narrates a listener the code doesn't
register, which is worse than fluff — a reader looking for the "beat per
event" behavior won't find it. The "keeps the structure symmetrical"
justification argues for a no-op the code declined to add. Either way the
asymmetry (JSON subscribes, text doesn't) is fine and simpler than the
comment implies.
**Suggested action:** Delete the stale "we still register a listener …
beat per event … keeps the structure symmetrical" paragraph
(`print.rs:322-328`) and keep a one-line note that text mode renders the
final message after `prompt` returns and needs no live listener.
**Effort:** S

### [Minor][Comments] Module/inline docs narrate `docs/aj-next-progress.md` step chronology and "legacy binary" framing — `src/aj/src/modes/print.rs:152-158,19-25`, `src/aj/src/persistence.rs:10-16`, `src/aj/src/modes.rs:1-9`
**What:** Recurring chronology theme (AG1/SE1/A1). `print.rs:152-158`
says the scripted path "keeps the legacy `Arc<dyn Model>` surface (step
6.8 of `docs/aj-next-progress.md` will port it onto `ScriptedProvider`)"
— a forward-looking step reference that has already happened (the code
*does* use `ScriptedProvider`, `scripted.rs:24,61`), so the comment is
both stale and a roadmap pointer. `print.rs:126,139` reference "the
legacy binary's behaviour/uses" as the authority for current behavior.
`persistence.rs:10-16` narrates "The scaffold leaves it empty; the print
mode step adds the first wiring helper" — but the module is still empty
and the helper lives in `aj-session`, so the comment describes a plan that
didn't land here. `print.rs:20-21` cites the protocol test by name
(useful) but also `docs/aj-next-plan.md §4.2` (`modes.rs:3`,
`print.rs:3`, `lib.rs:3`).
**Why it matters:** Comments must stand on their own; "step 6.8 will port
it" and "the scaffold leaves it empty; the print mode step adds…" are the
exact "§N lands later / scaffold fills in later" framing the guidance
forbids, and the "legacy binary" references point at a binary that is no
longer the source of truth. `persistence.rs`'s entire doc describes a
future that doesn't match the current empty-module reality.
**Suggested action:** Restate as steady-state: the scripted path uses
`ScriptedProvider`; the disabled-tools/precedence behavior is "the
binary's behavior" (drop "legacy"); `persistence.rs` either gets deleted
(see Nit) or its doc states what the module *is* (a reserved home for
`aj`-specific persistence decisions) without the scaffold narrative. Drop
the `§N`/step references. Resolve with the workspace-wide chronology pass.
**Effort:** S

### [Nit][Simplicity] `persistence.rs` is an empty doc-only module — either delete it or give it the wiring it documents — `src/aj/src/persistence.rs:1-16`, `src/aj/src/lib.rs:30`
**What:** `persistence.rs` contains only a module doc comment — no code.
It's `pub mod persistence;` in `lib.rs:30` and the `lib.rs` module list
describes it as "thin wrapper that builds the `aj-session` persistence
listener for either mode" (`lib.rs:14-15`), but the actual wiring is
`aj_session::persistence_listener` called inline at the two mode sites
(`print.rs:334`, `interactive.rs:537`). So the module is a placeholder
that neither owns the listener factory nor holds any `aj`-specific
persistence decision; it's dead public surface.
**Why it matters:** A `pub mod` that exists only to host a doc comment
about wiring that lives elsewhere is vestigial — the rubric's dead-code
item. The `lib.rs` doc advertising it as a "thin wrapper" overstates an
empty file. It's harmless but misleading: a reader expecting the
persistence seam finds nothing.
**Suggested action:** Either delete the module and its `pub mod`
declaration (the listener lives in `aj-session`, registered inline), or —
if the shared `SessionSetup` from the Major finding lands — move the
single `agent.subscribe(persistence_listener(…))` registration here so the
module earns its keep as the binary's persistence home. Decide with the
user; deletion is the smaller change.
**Effort:** S

### [Nit][Testing] Only `collect_prompt_text` is unit-tested; the whole `run` orchestration (resume handshake, error-bucket mapping, final-text rendering) has no test seam — `src/aj/src/modes/print.rs:483-573`
**What:** The in-module `#[cfg(test)]` suite covers `collect_prompt_text`
thoroughly (top-level vs `continue` prompt, empty-prompt error, the
ambiguous-lone-positional case) — genuinely good edge-case coverage of the
dispatch shape. But `run` itself — the resume + repair + seed handshake,
the `TurnError` → exit-code mapping (`print.rs:368-379`), and
`print_final_assistant_text`'s "no assistant message" error path
(`print.rs:467-471`) — has no test. The `--scripted` flag plus
`ScriptedProvider` is exactly the seam that would let a test drive `run`
end-to-end against a canned turn (A5's `replay_parity` test uses scripted
demos), but no in-crate test exercises it, and `print_final_assistant_text`
/ the error buckets are pure-ish functions that could be unit-tested
without I/O.
**Why it matters:** The happy-path-only theme: the most consequential
print-mode behavior (which message is printed, how a truncated/aborted
turn maps to an exit code) is untested at this layer. `print_final_assistant_text`
walking back for "the most recent assistant message" and erroring on none
is a contract worth pinning. The error-bucket mapping is the headless
caller's entire failure contract.
**Suggested action:** Add a unit test for `print_final_assistant_text`
over a hand-built transcript (last-assistant selection, multi-text-block
join, no-assistant error) and, if feasible, a scripted-provider `run`
test asserting text-mode prints the final answer and a `Recoverable`/`Aborted`
turn returns a non-zero/`Err`. The A5 step may cover the end-to-end seam;
flag the `print_final_assistant_text` unit gap regardless.
**Effort:** M

## What's good

- **Persistence is a pure subscriber and the boundary holds (confirms
  CLAUDE.md / SE1 / AG1).** `persistence.rs` holds no code;
  `print.rs:334` and `interactive.rs:537` register
  `aj_session::persistence_listener(Arc::clone(&log))` as just another
  bus subscriber. The binary owns the `ConversationLog`, wraps it in
  `Arc<TokioMutex<_>>` *after* the synchronous repair walk, and the
  listener never reaches back into the agent runtime. No back-coupling,
  exactly as the architecture claims.
- **Print mode does NOT depend on the vestigial `AgentEnd.messages` (AG1).**
  `print_final_assistant_text` walks `agent.messages()` directly
  (`print.rs:461`) rather than reading the always-empty `AgentEnd`
  snapshot field, so AG1's dead-field finding doesn't bite here. Text
  mode's "print the last assistant message's visible text blocks" contract
  is sourced from the live transcript, the authoritative place.
- **The print-mode error taxonomy is well-designed and documented.** The
  three `TurnError` buckets (Recoverable / Aborted / Fatal, `print.rs:336-379`)
  each map to a non-zero exit with a distinct context string so scripts can
  tell a cancel from a model failure from a disk failure. The block
  comment enumerating the buckets and their disk-consistency implications
  (`print.rs:336-357`) is the kind of contract documentation the guidance
  wants. Truncation-as-success is *not* re-introduced here — the binary
  faithfully surfaces whatever the runtime returns (the AG1 truncation gap
  is upstream, not in this layer).
- **SIGINT handling is careful.** The `ctrl_c` handler trips the turn's
  `CancellationToken` once, the handler is `abort()`ed before return so a
  stray Ctrl+C during shutdown can't phantom-cancel (`print.rs:358-367`),
  and the "second SIGINT exits hard" escape is documented. Stdout is
  explicitly flushed before exit so piped consumers don't lose buffered
  bytes (`print.rs:387-389`).
- **The JSON-mode listener ordering is deliberate.** Registering the JSONL
  sink *before* persistence (`print.rs:311-334`) so the user sees every
  event up to a persistence failure, and draining replayed historical
  events through the JSON sink *before* subscribing any bus listeners on
  resume (`print.rs:299-306`) so persistence doesn't re-write on-disk
  events, are both subtle correctness decisions with clear rationale.
- **`scripted.rs` is a clean, well-gated test seam.** `resolve_or_explain`
  is a one-shot resolver returning the same `(provider, model_info)` shape
  as the real-provider path so both call sites plug in without a
  conditional (`scripted.rs:13-17`); the `ExhaustedBehavior::EndTurn`
  default vs `Panic`-for-strict-tests choice is documented; the synthesized
  `ModelInfo` is zero-cost/text-only and namespaced `scripted/<name>` so the
  footer surfaces which flow runs. The catalog-help-and-exit UX (`help` /
  `list` / `?` → stderr + `exit(0)`; unknown → stderr + non-zero) is
  reasonable.
- **`collect_prompt_text` is well-tested at the boundary.** The dispatch
  edge cases (top-level vs `continue` prompt, the greedy-positional
  ambiguity, empty-prompt error) are covered with explanatory test
  comments — a model of edge-case-first unit testing for the one piece of
  print-mode logic that has a clean seam.

## Boundary & architecture notes

Dependency direction matches `CLAUDE.md`: the `aj` binary consumes the
public surfaces of `aj-conf`, `aj-models`, `aj-agent`, `aj-session`, and
`aj-tools` and never reaches the wrong way. The composition-root concern
is *internal* to the binary: there is no single place that assembles the
agent + tools + log + seeded transcript from `Args`/`Config`; instead the
two modes inline the same pipeline (Major finding). `persistence.rs` is
declared as the persistence home but is empty, while the actual listener
lives in `aj-session` and is registered inline — the seam the module doc
promises doesn't exist in code (Nit).

Public surface: `lib.rs` exposes every module `pub` (including the empty
`persistence`). `scripted::resolve_or_explain` and `ResolvedScriptedModel`
are `pub` and consumed by both modes — justified. `main.rs`'s subcommand
handlers are private free functions — correctly scoped.

For synthesis: (1) the in-binary session-setup duplication is the natural
home for A1's `ModelSelection` merge and the disabled-tools helper — all
three duplication findings point at the same missing `SessionSetup` seam.
(2) SE1's cross-process single-writer concern resolves here only insofar
as the binary spawns the second process; the binary opens each log once
per process, so it doesn't add an in-process double-open, but it also
doesn't guard the multi-process case.

## Test assessment

In-module `#[cfg(test)]` in `print.rs` per convention, exercising the
`collect_prompt_text` dispatch contract well (edge cases, error paths, the
clap greedy-positional ambiguity) through the real `Args::parse_from`
surface — a genuine boundary test. `scripted.rs` has no tests (its
behavior is `exit`/`bail` plumbing plus a pure `ModelInfo` builder; the
`scripted_model_info` shape would be a cheap unit test but is low-risk).
`main.rs`, `lib.rs`, `modes.rs`, and `persistence.rs` have no tests
(entrypoint/declaration/doc-only — acceptable, though the `.env`/logger
ordering in `main` is exactly the kind of startup logic that's hard to
test and went wrong, per the Major finding).

Gaps (see findings): the whole `print::run` orchestration — resume
handshake, the `TurnError` → exit-code mapping, and
`print_final_assistant_text`'s last-assistant selection + no-message error
— is untested at this layer despite the `--scripted` seam being available
(A5 may cover end-to-end). The JSON-mode write-failure handling
(`json_event_listener`) is untested. No flakiness risks in the existing
suite (pure arg parsing, no clock/network/fs).

## Cross-cutting themes to bubble up

- **No composition root in the binary (CONFIRMED, widest instance).** A1
  found the precedence merge (5 sites) and disabled-tools filter (3 sites)
  duplicated; this audit shows the *entire* session-setup pipeline
  duplicated between print and interactive (6+ identical blocks). All
  point at one missing `SessionSetup`/`build_session` seam. Synthesis
  should treat these as a single refactor, not three.
- **Startup sequencing: `.env` loaded after the logger (NEW).** `main.rs`
  initializes tracing before loading `~/.aj/.env`, so `RUST_LOG`/`AJ_LOG_FILE`
  from the documented env file don't apply to logging, and a bad
  `AJ_LOG_FILE` aborts the binary via `?`. The documented `.env`-first
  precedence holds for clap-read config env but silently not for logging.
- **Persistence-as-pure-subscriber boundary (CONFIRMED clean, third data
  point).** After AG1 (agent never touches a log) and SE1 (listener never
  calls back), the binary side confirms it: an empty `persistence.rs`, the
  listener registered as a plain subscriber, the log owned by the binary
  behind `Arc<TokioMutex<_>>`. The architecture's central claim holds end
  to end.
- **Single-writer / cross-process concurrency (CONFIRMED, unguarded at the
  spawn point).** SE1's no-lock concern: the binary opens each log once per
  process but is the layer that spawns the second `aj continue <id>`
  process, and guards nothing. The fix belongs near the resume call (or in
  `aj-session`); the binary at minimum should surface a lock error.
- **Truncation-looks-like-success (does NOT reproduce here).** Print mode
  faithfully maps the runtime's `TurnError` to non-zero exits and prints
  the live transcript; it doesn't paper over a truncated turn as success.
  The AG1 truncation gap stays upstream in the runtime/providers.
- **Chronology / roadmap-doc references in comments (CONFIRMED, recurs).**
  "step 6.8 will port it", "the scaffold leaves it empty; the print mode
  step adds…", "the legacy binary's behaviour", `§4.2` references — same
  theme as AG1/SE1/A1. `persistence.rs`'s entire doc describes a plan that
  didn't land in the file.
- **Dead/aspirational declared surface (CONFIRMED).** The empty
  `persistence.rs` module and the stale text-mode "no-op listener" comment
  describe behavior that isn't there; the `@file` `expand` call (A1) is a
  no-op stub wired into only print mode. Same shape as AG1's never-emitted
  event variants.
- **Happy-path-only coverage (CONFIRMED).** Only `collect_prompt_text` is
  tested; the consequential `run` orchestration and the headless error
  contract are untested despite an available scripted seam.
