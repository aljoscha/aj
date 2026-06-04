# Audit findings — aj-tests

- **Step:** A5
- **Date:** 2026-06-02
- **Audited commit:** 3dca263
- **Scope:** `src/aj/tests/replay_parity.rs` (the binary's only
  integration test), the binary-level boundary observations of the
  in-module `#[cfg(test)]` suites across `src/aj/src/` (29 files,
  ~250 test fns; sampled the model-precedence, print-dispatch,
  interactive-helper, and env-coupled suites), `src/aj/src/scripted.rs`
  (re-read for the `--scripted` determinism seam), and `src/aj/Cargo.toml`
  dev-deps. Cross-read: the `aj-models` `ScriptedProvider`, and the
  `aj-tui` `tests/support` harness (T5) for the reuse question.

## Summary

`replay_parity.rs` is the single strongest test in the binary and a
genuinely meaningful end-to-end boundary test: it drives one live
tool-call turn, simulates a crash between tool-result persistence and the
next inference, resumes from the on-disk log, and asserts the chat
scrollback rendered from the captured live `AgentEvent` stream is
**byte-for-byte identical** to the scrollback rendered from `replay(&log)`
— across three structured-rendering paths (`Bash`/`Diff`/`Todos`). It is
deterministic (scripted provider, `Duration::ZERO`, fixed env, `TempDir`s,
`ExhaustedBehavior::Panic` to make a runaway turn loud) and asserts on
*rendered output through the production pump + layout*, not internals.
This is exactly the SE1↔AG1↔rendering bridge the audit wanted.

But its parity is **scoped to the chat container only** (header/status/
footer are deliberately excluded), and the comparison compares the live
*event stream* against the replay event stream — both of which already
discard the persisted `ConversationEntry::timestamp` (SE1's Major). So
the test cannot, by construction, catch the replay-timestamp-loss bug:
both sides are equally lossy, so they agree. The bigger strategic gap is
the one A3 predicted: **there is no test that drives the interactive `run`
loop** — not cancellation, not the mid-turn-selector freeze (A3's Major),
not session swap, not the Ctrl+C/quit ladder — even though the
`--scripted` seam plus a virtual terminal make it feasible, and
`replay_parity.rs` proves the binary already knows how to stand up an
`Agent` + `EventPump` + layout headlessly. Coverage is otherwise the
familiar shape: leaf helpers and pure functions are well-tested at their
boundaries; the two most complex seams (the `run` loop, the multi-mode
startup pipeline) have none. The binary also re-rolls a no-op
`StubTerminal` rather than reusing aj-tui's far stronger `VirtualTerminal`
harness — which it *can't* depend on because the T5 harness lives in
`aj-tui/tests/support`, not a published test-support crate.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 2 | 4 | 1 |

## Findings

### [Major][Testing] No test drives the interactive `run` loop — the binary's most complex and most defect-prone seam is entirely uncovered — `src/aj/src/modes/interactive.rs:111-1262`, `src/aj/tests/replay_parity.rs`
**What:** A3 flagged that `InteractiveMode::run` is a ~1,150-line
`tokio::select!` god-loop and that it has no test of its own (only the
leaf helpers — `footer_data`, `shutdown`, `layout`, `editor_ext`,
`event_pump` — are covered). A5 confirms the integration-test gap: the
binary's *only* integration test (`replay_parity.rs`) drives an `Agent`
directly via `agent.prompt(...)` and feeds the captured events through a
*standalone* `EventPump` (`render_live`, `replay_parity.rs:319-325`) — it
never instantiates `InteractiveMode` or enters `run`. So none of the loop's
consequential behaviors are exercised anywhere: the per-turn
`CancellationToken` / Ctrl+C path, the mid-turn-selector agent-mutex
freeze (A3's Major lost-cancellation bug), the session-swap /
new-session rebind, the palette back-stack, the closed-channel `continue`
busy-spin (A3 Minor), the biased-select priority. The pieces to test it
already exist in-tree: `replay_parity.rs` proves the binary can stand up
`build_layout` + `EventPump` + a `Terminal` double headlessly, and
`scripted.rs` + `aj_models::scripted` give a deterministic multi-turn
provider seam that the `--scripted` CLI flag already wires into both modes.
**Why it matters:** Testing at the boundary — the rubric makes "a key
boundary with no test coverage" at least Major, and this is the binary's
single most intricate and most defect-laden seam (A3 found a real
lost-cancellation bug hiding precisely in this loop's nesting). The most
consequential frontend code is both the least readable and the least
covered; a regression in the cancel/quit/swap control flow would not fail
any test.
**Suggested action:** Add an integration test that drives a full
interactive turn against `--scripted`/`ScriptedProvider` with a virtual
terminal (the natural shape after A3's `run`-loop extraction lands): script
one turn, inject a submit input, assert the rendered scrollback; then a
cancellation case (fire the cancel token mid-turn, assert "Turn cancelled."
and a live session); then a session-swap case. Even a smoke test that
`run` starts, processes one scripted turn, and exits cleanly would guard
the startup→loop→teardown wiring.
**Effort:** L

### [Major][Testing] Replay-parity compares two equally-lossy event streams, so it cannot catch SE1's dropped-timestamp regression (and excludes header/status/footer) — `src/aj/tests/replay_parity.rs:204-216,384-391`
**What:** The parity assertion renders only the **chat container**
(`render_chat` reads `SlotIndex::Chat`, `replay_parity.rs:211-216`) and
explicitly excludes header/status/editor/footer because they "would
diverge for reasons unrelated to tool rendering (session id timestamps,
the loader's running-vs-idle state …)" (`:206-210`). That exclusion is
reasonable for *tool-render* parity, but it means the test asserts nothing
about the metadata slots — including any "sent at HH:MM" / turn-duration
rendering that SE1's Major (replay drops `ConversationEntry::timestamp`,
`replay.rs:100-281`) would corrupt. Worse, the comparison is
*live-event-stream vs replay-event-stream*: the captured live events
(`drive_live_turn`) and the `replay(&log)` events are **both** projected
without the persisted entry timestamp, so they agree *because both are
lossy in the same way*. A parity test structured this way can never
surface the timestamp-loss bug — the two sides drift in lockstep.
**Why it matters:** This is the SE1 cross-check the step was asked to run,
and it holds: parity testing as currently shaped **masks** the
timestamp-fidelity gap rather than catching it. The test's docstring
claims it guards the "scrollback looks different on resume" class of bug
(`replay_parity.rs:12-16`), but a whole sub-class of that bug
(time-dependent rendering) is structurally outside its reach. It's Major
because it's a false sense of coverage on a fidelity boundary the audit
treats as central, not because the chat-only parity it *does* assert is
wrong.
**Suggested action:** (1) Add a parity case that compares against the
*persisted log's recorded timestamps* rather than against the other
projection — e.g. assert the projected event carries
`entry.timestamp` (this is the SE1-suggested replay test and would fail
today). (2) Extend the parity comparison to the footer/status slots once
the loader-state and session-id divergences are pinned to deterministic
values (a fixed clock + fixed session id would let the metadata slots be
compared too), or document precisely which slots parity intentionally does
*not* cover and why.
**Effort:** M

### [Minor][Testing] The binary re-rolls a no-op `StubTerminal` instead of reusing aj-tui's `VirtualTerminal`, so parity never exercises the real frame compositor — `src/aj/tests/replay_parity.rs:82-112,211-216`
**What:** `replay_parity.rs` defines its own `StubTerminal` whose `write`
is a no-op (`:97`) and reads rendered lines directly from
`Container::render` (`:211-216`), bypassing `Tui`'s frame buffer / diff /
compositor entirely. T5 documented that aj-tui's `VirtualTerminal` is "the
strongest-engineered test harness in the workspace" — it implements the
real `Terminal` trait, feeds writes to a real VT100 parser, and lets tests
assert on the *composited* viewport, per-cell style, cursor, and
scrollback. The binary cannot reuse it: `VirtualTerminal` lives in
`aj-tui/tests/support` (a per-test-binary `#[path]` module), not a
published crate, so it isn't reachable from `aj`'s `tests/`. The binary's
only dev-dependency is `tempfile` (`Cargo.toml:38`); it has no path to the
harness.
**Why it matters:** Testing — the test-harness-reuse-vs-duplication theme
(T5). The parity test verifies that the *pump produces the same lines*,
which is the right target for tool-render parity, but because it reads
`Container::render` rather than the composited terminal output it can't
catch a divergence introduced by the `Tui` compositor / frame-diff (style
leak across slots, width clamping, scroll) — exactly the class
`VirtualTerminal` is built to catch. The no-op `StubTerminal` is a fourth
hand-rolled `Terminal` double in the workspace (cf. T5's
`ShrinkingTerminal` / `ShrinkOnSecondColumnsRead`), each re-implementing
the trait.
**Suggested action:** Promote the T5 harness to a shared test-support
crate (e.g. `aj-tui-testkit`, dev-dep-only) so both `aj-tui`'s suite and
the binary's integration tests share one `VirtualTerminal`. Then the
parity test can render through the real compositor and assert on the
composited viewport, and the interactive `run`-loop test (Major above)
gets a first-class terminal double for free. This is the concrete
"extract a shared test-support crate" recommendation the theme points to.
**Effort:** M

### [Minor][Testing] Env-mutating tests assume a single-threaded runner without `serial_test`, and one test panics if `HOME` is unset — flakiness latent in CI — `src/aj/src/modes/interactive.rs:3024,3046-3092`
**What:** `sandbox_warning_enabled_tracks_env_var_presence`
(`interactive.rs:3046`) mutates the process-global
`AJ_DISABLE_SANDBOX_WARNING` via `unsafe set_var`/`remove_var`, with a
safety comment asserting "tests in this module run on a single thread"
(`:3047-3055`). That assumption is false by default: `cargo test` runs the
tests *within a binary* on a multi-threaded pool, so a concurrently-running
test that also reads the env (or another env-mutating test) can interleave.
The binary uses **no `serial_test`** (confirmed: zero hits in `src/aj/`),
unlike T5's aj-tui suite which pairs every `EnvGuard` with
`#[serial_test::serial]`. Separately,
`build_context_notice_lists_files_with_label_and_tildified_path`
(`:3019`) does `std::env::var("HOME").expect("HOME set in test env")`
(`:3024`) — it hard-fails (panics) on any host where `HOME` is unset
rather than skipping or isolating, coupling the test to ambient
environment (C1's "AgentEnv reads real HOME" theme at the test layer).
**Why it matters:** Testing — hidden coupling to real env / ordering that
invites flakiness, exactly the rubric's flake category. The sandbox test's
safety comment documents an invariant the test framework does not
guarantee; the `HOME` test fails on a minimal CI image with `HOME` unset.
Neither is isolated to a tempdir or serialized.
**Suggested action:** Add `serial_test` as a dev-dep and mark the
env-mutating test `#[serial]` (matching T5's discipline), or refactor
`sandbox_warning_enabled` to take the env value as a parameter so the test
needn't touch process globals. For the `HOME` test, build the tildified
path against an injected `AgentEnv`/`display_path` base rather than the
live `HOME`, or skip when unset.
**Effort:** S

### [Minor][Testing] Print mode has no end-to-end driven-turn test — only CLI prompt-collection is covered — `src/aj/src/modes/print.rs:93-230,500-575`
**What:** `print::run` (`print.rs:93`) is the non-interactive turn driver:
it assembles the agent, registers the persistence listener, and runs a
turn to stdout (the A2 path). Its in-module tests cover only
`collect_prompt_text` argument-routing (5 tests, `print.rs:500-575`) — the
clap dispatch, not the run. There is no test that scripts a turn through
`print::run` and asserts the rendered stdout, even though print mode is the
simplest place to drive a full turn end-to-end (no TUI event loop) and the
`--scripted` seam is wired into print mode (`scripted.rs` doc,
`print.rs` plugs `ScriptedProvider` via the same resolver). `replay_parity`
exercises the *agent + pump* but not `print::run`'s assembly/output path.
**Why it matters:** Testing at the boundary — print mode is the binary's
batch entrypoint and the easiest full-turn integration target, yet only
its argument parsing is tested. A regression in print's agent assembly,
listener registration, or stdout rendering would not fail a test. This is
the same happy-path / untested-driver shape as the interactive finding, at
lower complexity.
**Suggested action:** Add an integration test that runs `print::run` (or a
testable inner fn) against `--scripted` with a captured stdout sink and
asserts the rendered turn output and that the session log was written.
Pair it with the `run`-loop test infrastructure so both modes share one
scripted-turn fixture.
**Effort:** M

### [Minor][Testing] Truncation-looks-like-success is unpinned at the binary boundary too — no test scripts an empty/partial `Done` through the pump — `src/aj/tests/replay_parity.rs`, `src/aj/src/modes/interactive/event_pump.rs`
**What:** AG1/M3/M4/SE1 flagged that a truncated turn the runtime accepts
as `Done` is persisted and replayed as a finalized `MessageEnd`; A3 noted
the pump faithfully renders it as a complete assistant message with no
marker, carrying no guard of its own, and that no test pins the behavior.
A5 confirms from the integration side: `replay_parity.rs` scripts only
well-formed single-tool-call turns (`one_tool_use_message`,
`:168-188`), and no in-module pump test scripts a `Done` with empty/partial
content. So neither the live nor the replay render of a truncated turn is
asserted anywhere in the binary.
**Why it matters:** Testing — the truncation cross-check, confirmed
unguarded at the consumer. The defect is upstream (the runtime treating
truncation as `Done`), but the binary's rendering of that state is the
user-visible surface and is undocumented and untested.
**Suggested action:** Add a pump test (in `event_pump.rs`) or a
parity-style integration case that scripts an empty/partial `Done` and
asserts the rendered scrollback, documenting the binary's current
(mis)handling so a future truncation-marker change has a baseline.
**Effort:** S

### [Nit][Comments] `replay_parity.rs`'s module doc cites a plan-doc step and narrates a migration — chronology theme — `src/aj/tests/replay_parity.rs:1-39`
**What:** The module doc opens "Per `docs/aj-next-progress.md` Step 5 of
the 'Resume fidelity follow-up' plan" (`:3`) and the `one_tool_use_message`
/ `drive_live_turn` comments reference "the legacy script shape (one
terminal event per inference) so the locked bus-event sequence is preserved
across the migration" (`:257-259`). Same chronology / external-doc-coupling
smell flagged in AG1/SE1/A1/A2/A3/T5: comments tied to a mutable plan-doc
step and to a migration narrative rather than stating the test's
steady-state contract.
**Why it matters:** Comments must stand on their own; the `§`/step
reference and "across the migration" framing go stale and force a reader to
find a doc that may not match HEAD. The *rationale* (why a kill switch, why
chunk_size 0, why `ExhaustedBehavior::Panic`) is excellent and worth
keeping — only the chronology pointers are noise.
**Suggested action:** Drop the "Per `docs/aj-next-progress.md` Step 5"
opener and the "across the migration" / "legacy script shape" framing;
keep the kill-switch and chunk-size rationale as steady-state prose.
Resolve with the workspace-wide chronology pass.
**Effort:** S

## What's good

- **`replay_parity.rs` is a real end-to-end boundary test, deterministic
  by construction.** It bridges SE1 (persistence + `replay`), AG1 (the
  agent turn loop + bus), and rendering (`EventPump` + production
  `build_layout`) in one assertion: the live-captured event stream and the
  resumed `replay(&log)` event stream must render the chat scrollback
  **byte-for-byte identically**, across the three `ToolDetails` paths.
  Determinism is taken seriously — scripted provider with `Duration::ZERO`,
  a fixed `empty_env` (no host probe, no `AGENTS.md` ingestion, fixed
  date), `TempDir` session/working dirs, and `ExhaustedBehavior::Panic` so a
  kill-switch that fails to bite panics immediately rather than passing
  silently. The `diff_lines` helper surfaces both transcripts in the panic
  message so a regression is debuggable from the failure alone.
- **The crash-simulation design is principled.** Registering three bus
  listeners in order (persistence → capture → kill switch) and failing the
  kill switch on `ToolExecutionEnd` so the earlier two have already run is a
  precise, faithful model of "process dies after tool result lands on disk
  but before the next inference" — and it asserts the turn bails with
  `TurnError::Fatal`, exercising AG1's error taxonomy at the boundary.
- **Asserts on rendered output, not internals.** Both passes go through the
  production pump and layout and compare `Container::render` line vectors;
  the test doesn't reach into pump/component private state. The fixtures
  (`build_tui_and_pump`, `render_chat`) are shared between the live and
  replay passes so the only variable is the event sequence — the right way
  to isolate the parity question.
- **The `--scripted` determinism seam is well-factored.** `scripted.rs`
  resolves the flag into the same `(provider, model_info)` shape as the
  real-provider path so both modes plug it into `Agent::with_provider`
  without a conditional, and the `EndTurn`-vs-`Panic` exhausted-behavior
  split (lenient for demos, strict for tests) is documented. This is the
  seam an interactive `run`-loop test would build on.
- **Leaf-helper and pure-function coverage is genuinely strong.** `model.rs`
  (15 tests: model-pick precedence, speed-header stamping, thinking-display
  fan-out), `config/theme.rs` (24), `session_selector` (21),
  `tool_execution`/`assistant_message` (16/15), `clipboard` (11,
  MIME negotiation), `prompt_history`/`footer`/`command_palette` — these
  exercise their boundaries with edge cases, and A3 separately confirmed
  `footer_data`/`shutdown`/`editor_ext` as exemplary. The placement is
  correct per `CLAUDE.md` (in-module `#[cfg(test)]` for the binary).

## Boundary & architecture notes

`replay_parity.rs` confirms the CLAUDE.md ownership claim from the test
side: the *test* owns the `ConversationLog`, creates the
`ConversationPersistence`, and registers `persistence_listener(...)` as a
plain bus subscriber via `agent.subscribe(...)` — `Agent::prompt` takes no
`&ConversationLog`. This is the same composition the binary's two modes use
(A2/A3), so the test is a faithful miniature of the production wiring.

The dependency surface the test reaches is exactly the binary's: it
imports public items from `aj-agent`, `aj-session`, `aj-models`,
`aj-tools`, `aj-tui`, and `aj-conf`, plus the binary's own
`aj::config::theme`, `aj::modes::interactive::{event_pump, layout}`. That
those binary internals (`EventPump`, `build_layout`, `SlotIndex`,
`chat_theme`) are reachable from an integration test means they're `pub`
— A3's Nit about over-broad `pub` on the leaf modules is what makes the
integration test possible. Worth recording for synthesis: the binary's
`pub` surface is partly load-bearing *for its own integration test*, so a
`pub(crate)` tightening pass must keep an integration-test seam (another
argument for a shared test-support crate that re-exports the needed pieces).

For synthesis: the missing shared test-support crate is the structural
cause of both the `StubTerminal` duplication here and T5's observation that
`VirtualTerminal` is trapped in `aj-tui/tests/support`. One
`aj-tui-testkit` (or workspace `test-support`) dev-dep crate would let the
binary's integration tests render through the real compositor and would
give the proposed `run`-loop test a first-class terminal double.

## Test assessment

The binary's testing splits cleanly into two tiers with opposite verdicts.

**Pure / leaf tier (good).** ~250 in-module tests across 29 files cover
the model-precedence logic, CLI prompt-collection, theme resolution,
clipboard MIME negotiation, the selector/component widgets, and the
interactive leaf helpers — at their boundaries, with edge cases, correctly
placed in-module. These refute "happy-path-only" for the *leaf* surfaces.

**Driver / loop tier (the gap).** The two highest-complexity seams have no
integration coverage: the interactive `run` loop (A3's ~1,150-line
`select!`, including the lost-cancellation freeze, session swap, palette
back-stack, closed-channel spin) and `print::run`'s full turn-to-stdout
path. `replay_parity.rs` drives the `Agent` + pump + layout but never
enters either mode's `run`. The infrastructure to close this gap exists
in-tree (the scripted seam + the headless layout/pump setup
`replay_parity.rs` already demonstrates); the obstacle is the absent
shared terminal harness and the un-extracted `run` loop (A3 Major).

**Parity test quality.** Strong and deterministic, but two structural
limits: (1) chat-container-only — header/status/footer parity is out of
scope by design, so time-dependent metadata rendering is unasserted; (2)
it compares two equally-timestamp-lossy event streams, so it *cannot*
catch SE1's dropped-timestamp regression — a false-coverage hazard on a
fidelity boundary. It also reads `Container::render` rather than the
composited terminal, so `Tui`-compositor divergences are out of reach.

**Flakiness.** The integration test is clean (no clock/network/real-env
coupling beyond `TempDir`s). The in-module risk is the env-mutating
`sandbox_warning_enabled` test (no `serial_test`, false single-thread
assumption) and the `HOME`-dependent context-notice test (panics if `HOME`
unset) — both latent CI-flake/failure sources, against the grain of T5's
disciplined RAII-env-guard + `serial` pattern.

## Cross-cutting themes to bubble up

- **Happy-path-only / untested complex seams (CONFIRMED, sharpest
  instance).** Leaf helpers and pure functions are well-tested; the
  binary's two `run` drivers (interactive loop, print turn) have zero
  integration coverage despite an available scripted + headless-layout
  seam. This is A3's prediction confirmed from the integration side and the
  workspace's clearest case of "the most consequential code is the least
  tested."
- **Replay timestamp loss (CONFIRMED, and parity testing masks it).**
  SE1's dropped-`ConversationEntry::timestamp` is structurally invisible to
  `replay_parity.rs` because it compares two equally-lossy event streams —
  parity holds *because* both sides discard the timestamp. The fidelity
  boundary the test claims to guard has a hole the test cannot see.
- **Test-harness reuse vs duplication (CONFIRMED → concrete
  recommendation).** The binary rolls its own no-op `StubTerminal` because
  T5's `VirtualTerminal` is trapped in `aj-tui/tests/support`. Extracting a
  shared dev-dep test-support crate (`aj-tui-testkit`) would let the binary
  render through the real compositor *and* unblock the missing `run`-loop
  test. Ties to the "test-harness-shipped-in-prod" theme.
- **Scripted provider as the determinism seam (CONFIRMED, working).** The
  `--scripted` path and `ScriptedProvider` are the binary's determinism
  primitive and are well-factored; `replay_parity.rs` uses them correctly
  (`Duration::ZERO`, `ExhaustedBehavior::Panic`). The seam is ready for the
  un-built `run`-loop and print-turn tests.
- **Wall-clock & real-env coupling (CONFIRMED at the test layer).** Two
  in-module tests couple to process-global env / live `HOME` without
  `serial_test` or isolation, unlike T5's RAII-guard + `serial` discipline.
  C1's "AgentEnv reads real HOME" propagates into the test suite.
- **Over-broad `pub` is load-bearing for tests (NEW nuance for synthesis).**
  A3's Nit (leaf modules `pub` where `pub(crate)` would do) is what lets
  `replay_parity.rs` reach `EventPump`/`build_layout`/`SlotIndex`. A
  tightening pass must preserve an integration seam — another argument for
  a shared test-support crate that re-exports the needed pieces.
- **Chronology in comments (CONFIRMED, recurs).** `replay_parity.rs` cites
  `docs/aj-next-progress.md` Step 5 and narrates "across the migration" /
  "legacy script shape" — same smell as AG1/SE1/A1/A2/A3/T5.
