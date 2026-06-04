# Audit findings — aj-interactive

- **Step:** A3
- **Date:** 2026-06-02
- **Audited commit:** 5030a5a
- **Scope:** `src/aj/src/modes/interactive.rs`,
  `src/aj/src/modes/interactive/event_pump.rs`,
  `src/aj/src/modes/interactive/layout.rs`,
  `src/aj/src/modes/interactive/keys.rs`,
  `src/aj/src/modes/interactive/editor_ext.rs`,
  `src/aj/src/modes/interactive/footer_data.rs`,
  `src/aj/src/modes/interactive/shutdown.rs` (incl. the in-module
  `#[cfg(test)]` suites in `event_pump.rs`, `editor_ext.rs`,
  `footer_data.rs`, `layout.rs`, `shutdown.rs`, and `interactive.rs`).

## Summary

The interactive frontend is functionally rich and its small leaf
modules (`footer_data`, `layout`, `shutdown`, the `PromptHistory` in
`editor_ext`) are clean, well-documented, and genuinely well-tested at
the boundary. The concurrency *skeleton* is sound — a single biased
`tokio::select!` multiplexes the agent-turn `JoinHandle`, an OAuth-login
`JoinHandle`, terminal input/render, an unbounded agent-bus mpsc
channel, and the theme-watcher channel; the unbounded channel means no
lagging-subscriber loss (cf. AG1) and the per-turn `CancellationToken`
threads Ctrl+C straight into AG1's three-checkpoint cancel design.

But `interactive.rs` is a 3,116-line god-module whose `run` method is a
~1,150-line loop, and the audit's predicted themes all reproduce here.
The biggest concrete defect: the agent's `TokioMutex` is held for the
**entire turn** by the spawned prompt task, and several mid-turn
code paths (`/thinking` and `/model` reached via the `Ctrl+O` palette)
`agent.lock().await` — which suspends the whole select loop until the
turn ends, freezing input and making Ctrl+C cancellation impossible
during that window. The session-setup pipeline A2 flagged is duplicated
not twice but **four** times within this file (startup + palette
follow-up paths + `perform_session_swap` + `perform_new_session`).
`keys.rs` is an empty doc-only stub while the keybinding dispatch it
promises sprawls inline across ~200 lines of the run loop. Dead event
arms (AG1's never-emitted `TurnEnd`/`QueueUpdate`, and a *fully
implemented* `ToolExecutionUpdate` handler for an event the runtime
never emits) and chronology comments recur.

## Severity counts

| Critical | Major | Minor | Nit |
|---|---|---|---|
| 0 | 4 | 5 | 2 |

## Findings

### [Major][Simplicity] The agent `TokioMutex` is held for the whole turn, so a mid-turn selector that locks the agent freezes the event loop and defeats Ctrl+C cancellation — `src/aj/src/modes/interactive.rs:1162-1165,1895-1897,2287-2300`
**What:** The submit handler spawns the turn as
`tokio::spawn(async move { let mut a = agent_for_run.lock().await; a.prompt(trimmed, turn_cancel).await })`
(`interactive.rs:1162-1165`) — the `TokioMutex` guard is held across the
*entire* `prompt().await`, i.e. for the full duration of the turn. The
`Ctrl+O` command-palette open is gated only on
`open_selector.is_none() && login_session.is_none()`, **not** on
`running_task` (`interactive.rs:850-862,927-930`), so the palette opens
while a turn is running. Selecting `/thinking` from it routes a
`follow_up` into `handle_slash_command`, whose `OpenThinkingSelector`
arm does `let a = agent.lock().await; a.default_thinking()`
(`interactive.rs:1894-1897`); confirming a `/model` swap likewise locks
the agent (`interactive.rs:2373-2378`). Because the main loop `.await`s
that lock from inside the `tui.next_event()` arm, the whole
`tokio::select!` is suspended — no input, no render, and crucially no
processing of the Ctrl+C arm — until the in-flight turn finishes and
the spawned task releases the guard.
**Why it matters:** This is a real responsiveness / lost-cancellation
bug, not just a smell: during the freeze the user cannot cancel the turn
(the `current_turn_cancel` fire lives in the same blocked input arm) and
the UI appears hung. It is reachable through a documented affordance
(`Ctrl+O` → pick a setting) at exactly the time a user is most likely to
want it (a long turn they're reconsidering). The contradictory comment
"dispatch before checking `running_task` so `/quit` works even mid-turn"
(`interactive.rs:1096-1098`) is itself wrong — `disable_submit`
(`event_pump.rs:902-905`, set at `interactive.rs:1149`) blocks editor
submissions while a turn runs, so typed slash commands *can't* fire
mid-turn anyway; only the un-gated `Ctrl+O`/`Ctrl+R` chords can, and
those are the ones that hit the lock.
**Suggested action:** Don't hold the agent mutex across the whole turn:
have the spawned task lock, take what it needs, and structure
`prompt` so the long await doesn't pin the guard — or give the binary a
cancellation/cheap-read path that doesn't contend on the same mutex
(e.g. store `default_thinking`/`model_info` snapshots outside the
mutex, refreshed on change). At minimum, gate the `Ctrl+O`/`Ctrl+R`
chords and the agent-locking selector arms on `running_task.is_none()`
so they can't be entered mid-turn, and fix the misleading "/quit works
mid-turn" comment.
**Effort:** M

### [Major][Simplicity] `InteractiveMode::run` is a ~1,150-line god-method and `interactive.rs` a 3,116-line god-module — `src/aj/src/modes/interactive.rs:111-1262`
**What:** A single `async fn run` spans `interactive.rs:111-1262`: it
inlines model/auth resolution, log resume, system-prompt freeze, replay,
TUI construction, the entire `tokio::select!` loop, the Ctrl+C/Ctrl+D
key interception (`688-739`), four separate keybinding-match blocks each
re-acquiring the keybindings lock (`745-879`), the palette/history
synthetic-dispatch paths (`918-994`), selector-outcome polling, the
submit path, and the shutdown banner. The file additionally hosts the
`OpenSelector` state machine, `handle_slash_command` (~390 lines,
`1856-2247`), `handle_selector_outcome` (~470 lines, `2256-2732`),
`perform_session_swap`, `perform_new_session`, and a dozen free
helpers. This is the T1/T3 god-object theme at binary scope: one
function owns startup, the concurrency core, key dispatch, overlay
lifecycle, and teardown.
**Why it matters:** The loop body's nesting (a `match event` inside an
arm of a `select!`, with `if let … { match … { … } }` several levels
deep) makes the control flow — especially the `continue`/`break`
interplay that governs cancellation and quit — very hard to verify, and
it's exactly where the cancellation-freeze bug above hides. There is no
test for the loop itself (only the leaf helpers are tested), so the most
consequential code in the frontend is both the least readable and the
least covered.
**Suggested action:** Extract the cohesive pieces the file already
gestures at: a key/chord-dispatch unit (the natural home is the empty
`keys.rs`, see below), an overlay/selector controller owning
`OpenSelector` + `handle_slash_command` + `handle_selector_outcome`, and
a startup-assembly step shared with print mode (next finding). Shrink
`run` to: build session, register listeners, drive the select loop by
delegating each arm to a named handler.
**Effort:** L

### [Major][Simplicity] The session-setup pipeline is duplicated four times within this file (confirming and widening A2) — `src/aj/src/modes/interactive.rs:159-357,2778-2829,2900-2932`
**What:** A2 found the startup pipeline duplicated between `print::run`
and `InteractiveMode::run`. From the interactive side it is worse: the
same resume → repair → re-linearize → `seed_messages` →
system-prompt-reuse/freeze → `seed_sub_agent_counter` dance appears
**four** times in this one file: the startup path
(`interactive.rs:294-357`), `perform_session_swap`
(`2778-2829`), and `perform_new_session` (`2900-2932`), plus the
tool-assembly + precedence-merge + `Agent::with_provider` block is
itself written twice in the scripted-vs-registry branch
(`159-248`, with the disabled-tools filter and
`apply_thinking_display` copied verbatim in both arms). The
"build the new persistence subscription before dropping the old one"
ordering rule is also duplicated and commented identically in the two
swap helpers (`2831-2846`, `2934-2946`).
**Why it matters:** Six-plus near-identical blocks must stay in lockstep,
and they already differ in subtle ways (startup re-linearizes after
repair and `expect`s a post-repair head at `350-353`; the swap helper
*doesn't* re-linearize, `2785-2800`, so a repair that synthesizes a
tool_result would be missed on swap). The `Arc<TokioMutex<ConversationLog>>`
swap + fresh-`EventPump` + replay sequence is copy-pasted between swap
and new-session with only the create-vs-resume head differing. This is
the strongest case yet for the `SessionSetup`/`build_session`
composition root A1/A2 proposed — it would collapse the startup branch
*and* give the two in-process swap paths a single "rebind log + reseed
agent + re-replay" primitive.
**Suggested action:** Extract (1) a `build_session(args, config) ->
(Agent, ConversationLog, …)` shared with print mode, and (2) a
`rebind_session(agent, log_slot, persistence_handle, pump, new_log)`
used by both `perform_session_swap` and `perform_new_session`. Fold the
scripted/registry tool+model assembly into one helper so the
disabled-tools filter and `apply_thinking_display` live once.
**Effort:** L

### [Major][Boundaries] `keys.rs` is an empty doc-only stub while the keybinding dispatch it promises sprawls inline in the run loop — `src/aj/src/modes/interactive/keys.rs:1-7`, `src/aj/src/modes/interactive.rs:745-879`
**What:** `keys.rs` contains *only* a module doc comment: it claims to
"wrap `aj_tui::keybindings` with `aj`-level bindings: double-Esc cancel +
clear-queues, `Ctrl+O` to expand sub-agent transcripts, `Ctrl+C` to
interrupt a running turn" and closes with "Filled in by the
'Interactive TUI' step in Phase 1." None of that lives here. The actual
chord handling is inline in `run`: four consecutive blocks each doing
`let kb = aj_tui::keybindings::get(); if kb.matches(&input, ACTION_…) { drop(kb); … continue }`
for thinking-toggle, tools-expand, clipboard-paste, and
palette/close-all/history (`interactive.rs:745-879`), plus the hand-rolled
Ctrl+C/Ctrl+D priority ladder (`688-739`). The doc also describes
behavior that doesn't exist ("double-Esc cancel + clear-queues",
"`Ctrl+O` to expand sub-agent transcripts" — `Ctrl+O` opens the command
palette here, `event_pump.rs:411-421` notes sub-agent grouping is
unimplemented), which is the T3 keybinding-doc-vs-reality mismatch.
**Why it matters:** Same vestigial-declared-surface smell as A2's empty
`persistence.rs`: a `pub mod` that exists only to host a doc describing
work done elsewhere (or not at all). It actively misleads — a reader
looking for the keymap finds an empty file whose doc promises bindings
that aren't implemented, while the real dispatch is buried in a god-loop
re-locking the global keybinding manager four times in a row.
**Suggested action:** Either delete `keys.rs` and its `pub mod`
declaration, or move the chord dispatch into it as a real
`fn dispatch_chord(tui, input, &mut LoopState) -> ChordOutcome` so the
run loop calls one function and the four repeated `kb.get()/matches/drop`
blocks collapse. Rewrite the doc to match actual bindings (no double-Esc,
`Ctrl+O` = palette) and drop the "Filled in by … Phase 1" chronology.
**Effort:** M

### [Minor][Simplicity] The event pump carries a fully-implemented handler for `ToolExecutionUpdate`, which AG1 confirmed the runtime never emits, plus no-op `TurnEnd`/`QueueUpdate` arms — `src/aj/src/modes/interactive/event_pump.rs:354-361,411-421,718-736`
**What:** AG1 found `TurnEnd`, `QueueUpdate`, and `ToolExecutionUpdate`
are declared but never emitted. Here the pump matches all three: `TurnEnd`
and `QueueUpdate` are explicit no-op placeholders (`event_pump.rs:411-421`,
documented as "land in follow-up commits") while
`ToolExecutionUpdate` has a *real* handler
(`354-361` dispatching to `update_tool_execution_partial`, a complete
~18-line method at `718-736`) for an event the runtime's `emit_update`
no-ops away. So the consumer holds dead code: a working partial-update
path that can never run, plus two placeholder arms.
**Why it matters:** Confirms AG1's "declared in the contract, never
emitted/used" theme at the consuming end. A reader of the pump
reasonably concludes tool-progress streaming works (the handler is
fully written); it doesn't. The no-op arms' justification ("keeps the
exhaustiveness check active") is reasonable for *keeping* an arm, but
the comment narrates a roadmap ("land in follow-up commits") rather than
the steady-state contract.
**Suggested action:** Resolve jointly with AG1's decision on the three
variants. If `ToolExecutionUpdate` is genuinely roadmap, keep a
single-line no-op arm and delete the unreachable
`update_tool_execution_partial` body (or `#[cfg]`-gate it) so it isn't
mistaken for live behavior. Replace the "land in follow-up commits"
comment with a statement of what the runtime currently emits.
**Effort:** S

### [Minor][Comments] `event_pump.rs` and `interactive.rs` narrate past/future states and roadmap-doc sections — `src/aj/src/modes/interactive/event_pump.rs:1-16,486-505`, `src/aj/src/modes/interactive.rs:137-145,449-451`
**What:** Recurring chronology theme (AG1/SE1/A1/A2).
`handle_message_start`'s doc spends ~20 lines on "Earlier versions of
this method pre-created the assistant component here … Deferring
creation … removes the orphan" (`event_pump.rs:486-505`) — a change
narrative. The pump module doc and several inline comments cite
`docs/aj-next-plan.md §1.6/§1.1/§4` and "richer sub-agent grouping lands
alongside the Ctrl+O expand affordance in a follow-up" (`event_pump.rs:9-16`).
`interactive.rs:137-145` repeats A2's "step 6.8 of
`docs/aj-next-progress.md` will port it onto `ScriptedProvider`" (stale —
the code already uses `ScriptedProvider`), and `449-451` says "richer
state lands alongside `footer_data` in a follow-up" (footer_data exists
and is wired). Several comments reference "the legacy binary" as the
authority for current behavior (`event_pump.rs:817`, `shutdown.rs:9-11`).
**Why it matters:** Comments must stand on their own; "earlier versions
did X", "§N lands later", and "the legacy binary" couple the code to a
mutable roadmap and to a binary that's no longer the source of truth.
The §-references are the same external-doc coupling AG1/A2 flagged.
**Suggested action:** Restate as steady-state. Keep the *rationale* for
lazy component creation (orphan-spacer avoidance) but drop the "earlier
versions" framing; drop the `docs/aj-next-*` § citations and the
"legacy binary" authority; remove the stale "step 6.8 will port"/"lands
in a follow-up" lines. Resolve with the workspace-wide chronology pass.
**Effort:** S

### [Minor][Comments] `editor_ext.rs`'s module doc claims four responsibilities but the module only implements `PromptHistory` — `src/aj/src/modes/interactive/editor_ext.rs:1-13`
**What:** The module doc says it "turn[s] the bare editor into the
prompt surface: slash-command completion, `@file` autocomplete …,
prompt-history wiring, and multi-line submit handling." In fact only
`PromptHistory` lives here; slash-command/palette triggering and `@file`
autocomplete are wired inline in `interactive.rs` (`397-447`) against
`aj_tui::autocomplete`, and submit handling is in the run loop. The doc
describes a module that doesn't exist around the one type that does.
**Why it matters:** Overstated module doc — a reader expecting the
autocomplete/slash plumbing here finds only history. Minor but it's the
same "declared surface ≠ code" shape as the `keys.rs` finding.
**Suggested action:** Trim the doc to what the module owns
(`PromptHistory` bootstrap from JSONL logs) or actually relocate the
editor-extension wiring here if the intent is for this to be the
editor-extension home. The `PromptHistory` type and its tests are good —
leave them.
**Effort:** S

### [Minor][Contracts] A closed agent-bus channel makes the `recv_event` select arm busy-spin — `src/aj/src/modes/interactive.rs:1172-1175,1269-1271`
**What:** `recv_event` returns `rx.recv().await`, and the arm treats
`None` (channel closed) as `continue` with the comment "transient blip;
keeps the TUI alive" (`interactive.rs:1172-1173,1268`). On a *closed*
unbounded channel `recv()` returns `None` immediately and forever, so if
the channel ever closes the biased `select!` will select this
ready-immediately arm whenever the higher-priority input arm is pending,
spinning the loop at 100% CPU. In normal operation the channel can't
close (the `_event_handle` subscription is held for the whole run), so
this is latent, but the "transient blip" framing misreads a permanent
condition as recoverable.
**Why it matters:** A closed-channel `None` is terminal, not transient;
the recovery comment is wrong and the fallback would hot-spin rather
than degrade gracefully. Contract mismatch on an edge that's currently
unreachable only by construction (the handle outliving the loop).
**Suggested action:** On `recv_event` returning `None`, either break the
loop (the agent is gone — quit like the `tui.next_event() == None` arm
does) or stop polling that arm (set the receiver to a pending-forever
state) instead of `continue`. Fix the comment to say the channel
shouldn't close while the subscription handle is alive.
**Effort:** S

### [Minor][Simplicity] The biased `select!` prioritizes input/render over agent-bus events, which can delay turn rendering under input pressure — `src/aj/src/modes/interactive.rs:593-594,681-1175`
**What:** The loop is `tokio::select! { biased; … }` with arm order
task-done → login → `tui.next_event()` → `recv_event` (agent bus) →
theme (`593-1185`). `biased` means a ready `tui.next_event()` always
wins over a ready agent-bus event. During a turn the agent emits many
bus events (each `pump.handle` then `request_render`), and render events
are produced on a throttle; under a stream of input/render readiness the
lower-priority `recv_event` arm can be repeatedly deprived, deferring
the visible streaming of assistant output until input quiesces.
**Why it matters:** Not a correctness bug (the unbounded channel never
drops events, so ordering within the bus is preserved and everything is
eventually pumped), but the priority choice means agent output can
visibly lag behind a user mashing keys. Worth a deliberate decision
rather than an accident of arm order.
**Suggested action:** Confirm the priority is intentional (input
latency over output latency) and document *why* `biased` and this order
were chosen at the `select!`. If turn-output responsiveness matters
more, reorder or drop `biased`. Low urgency.
**Effort:** S

### [Nit][Boundaries] Several `pub` items on the leaf modules could be `pub(crate)` — `src/aj/src/modes/interactive/event_pump.rs:130,152,164,…`, `shutdown.rs:30,40,98,129`, `footer_data.rs:43,53,69,76`
**What:** `EventPump`'s methods, `FooterData`'s API, and `shutdown`'s
`build_usage_summary`/`format_usage_summary`/`format_resume_hint` are
`pub`, but the binary crate is the only consumer (these aren't a library
surface). Tests reach them via `super::`/sibling-module paths that
`pub(crate)` would still satisfy.
**Why it matters:** Minimal-surface hygiene; in a binary it's harmless
but a wider-than-needed `pub` obscures which items are genuinely the
module's contract vs. internal helpers exposed only for the sibling
loop/tests.
**Suggested action:** Demote to `pub(crate)` where only in-crate code
(incl. tests) uses them. Low priority.
**Effort:** S

### [Nit][Simplicity] The four mid-loop keybinding blocks each re-acquire the global keybindings manager separately — `src/aj/src/modes/interactive.rs:745-879`
**What:** Thinking-toggle, tools-expand, clipboard-paste, and the
palette/close-all/history interception each open with
`let kb = aj_tui::keybindings::get(); … kb.matches(…); drop(kb);`
(`745-759`, `765-776`, `793-813`, `831-879`) — four sequential
acquire/match/drop cycles on the same shared manager for one keystroke.
**Why it matters:** Repetition and four lock round-trips per key; folds
naturally into the single chord-dispatch function the `keys.rs` finding
proposes (acquire once, match against an action table).
**Suggested action:** Fold into the `keys.rs` dispatch extraction:
acquire the manager once, match the input against the relevant actions,
return the chosen `ChordOutcome`.
**Effort:** S

## What's good

- **`footer_data.rs` is a model derived-state seam.** A `Copy` scalar
  snapshot with a crisp contract (numerator = `turn_input +
  turn_cache_read + turn_cache_write`, output excluded), the split that
  keeps the `Footer` component free of wire concerns and the pump free
  of display semantics, and four focused unit tests (`fresh → None`,
  prompt-component sum, denominator-only swap, last-turn-replace). This
  is the clean "derived view" the A1 footer concern wanted.
- **`shutdown.rs` is pure and well-tested.** The usage summary is built
  from primitive `Usage` values via `build_usage_summary_from_parts`
  (split out specifically so tests don't need a live `Agent`),
  sub-agent rows are sorted by id for deterministic output, and the
  byte-for-byte legacy format plus the resume-hint shape are pinned by
  tests. The print helpers' visual-rhythm rationale (leading blank,
  per-row dim wrap, one-column indent) is documented as contract, not
  fluff.
- **`layout.rs` is a tidy slot abstraction.** `SlotIndex` centralizes the
  numeric mapping so a reshape touches one match; `build_layout`
  deliberately doesn't call `tui.start()` so tests drive it against a
  virtual terminal. Small, single-responsibility, tested.
- **`PromptHistory` (editor_ext) is robust and thoroughly tested.** The
  JSONL-as-source-of-truth rationale (failure isolation per line,
  arbitrary-byte round-trip, one-writer-per-file) is documented as a
  real contract, and the suite covers chronological order, invalid-UTF-8
  skip, unparseable-JSON skip, null-byte/multiline round-trip,
  consecutive-only dedup, cap, subagent/assistant/tool-result exclusion,
  missing dir, and the editor-install ordering via observable `Up`
  presses. Exemplary edge-case-first testing.
- **Cancellation wiring matches AG1's design.** The binary mints a fresh
  per-turn `CancellationToken`, keeps a clone in `current_turn_cancel`,
  and fires it from the Ctrl+C arm without locking the agent
  (`interactive.rs:717-721,1160-1161`); the `Aborted` join outcome
  surfaces a "Turn cancelled." notice and keeps the session alive
  (`603-612`). This is the binary end of AG1's three-checkpoint cancel,
  and the `biased` select gives the agent-finished arm priority. (The
  freeze in the Major finding is orthogonal — it's the *agent mutex*,
  not the cancel token, that blocks.)
- **The event-pump spinner refcount is carefully reasoned.** The
  `running_agents` set with start-on-0→1 / stop-on-1→0 plus the
  authoritative Main-`AgentEnd` drain (`event_pump.rs:264-316`) correctly
  handles nested sub-agent `AgentStart`/`AgentEnd` and a dropped
  sub-agent end without pinning the render loop, with the soundness
  argument written down. Sub-agent vs. main footer separation
  (`397-409`) is likewise principled and tested.
- **Unbounded mpsc forwarder avoids AG1's lagging-loss concern.**
  `subscribe_channel` feeds an `UnboundedReceiver`, so the UI consumer
  can never drop agent events under backpressure — the right choice for
  a frontend that must render every event.
- **Normal-exit terminal restore + banner ordering is correct.** Every
  loop exit (quit, Ctrl+D, Fatal, panic-join) falls through to
  `running_task.abort()` → drop watcher → `tui.stop()` → print summary,
  and the banner is deliberately printed *after* `stop()` so it lands in
  the real shell scrollback (T1's panic-restore covers the abnormal
  path; this is the clean normal-exit restore). Persistence is a
  synchronous-per-event bus listener, so there's nothing buffered to
  flush at teardown.

## Boundary & architecture notes

Dependency direction is correct: the interactive mode consumes the
public surfaces of `aj-agent`, `aj-session`, `aj-models`, `aj-conf`,
`aj-tools`, and `aj-tui`, and persistence is registered as a plain bus
subscriber (`interactive.rs:535-538`), confirming A2's "binary owns the
log, registers a listener" verdict from the interactive side. The agent
and log are wrapped in `Arc<TokioMutex<_>>` *after* the synchronous
repair walk (`526-527`), matching print mode.

The internal boundary problems are: (1) no composition root — startup
assembly is duplicated four ways within this file and twice more vs.
print mode (Major); (2) `keys.rs` is declared but empty while its
responsibility sprawls inline (Major); (3) `interactive.rs` is a
god-module concentrating startup, the concurrency core, key dispatch,
overlay lifecycle, and teardown in one ~3k-line file (Major). For
synthesis: all three reinforce A1/A2's single missing `SessionSetup`
seam and add a missing key-dispatch / overlay-controller seam.

Public surface is wider than needed on the leaf modules (Nit) but
harmless in a binary.

## Test assessment

In-module `#[cfg(test)]` per convention, and the leaf modules are well
covered at the boundary: `footer_data` (4 tests, edge cases),
`shutdown` (5 tests incl. sort order + format pins), `layout` (slot
count), `editor_ext` (10 tests, strong edge/error coverage), and
`event_pump` has a good set — the usage-line formatter, the
main-vs-sub footer routing, context-window swap, and a named regression
test (`replay_tool_result_without_start_does_not_steal_next_assistant_message`)
that pins the resume-fidelity reorder fix through the public `handle`
surface against a real layout.

The gap is the same happy-path-only theme: **the `run` loop itself has
no test** — not the cancellation path, not the mid-turn-selector
interaction (the Major freeze), not the Ctrl+C/Ctrl+D/quit ladder, not
the session-swap/new-session rebind, not the palette back-stack
restore. These are the most consequential and most intricate behaviors
in the frontend, and the `--scripted` provider seam plus a virtual
terminal would make a driven `run` test feasible (A5 may cover some end
to end). Truncation-as-`Done` (M3/M4/AG1) is untested here too — the
pump would render a truncated turn as a finished assistant message, so a
test scripting an empty/partial `Done` would document the UI's current
(mis)handling. No flakiness risks in the existing suite (the
`editor_ext` scratch dirs are uniquely named; no clock/network coupling
beyond temp-file paths).

## Cross-cutting themes to bubble up

- **No composition root (CONFIRMED, widest instance yet).** The
  session-setup pipeline is duplicated *four* times inside
  `interactive.rs` alone, plus twice more vs. print mode — generalizing
  A1/A2's precedence-merge and disabled-tools findings. One
  `build_session` + `rebind_session` refactor subsumes all of it.
- **God-objects (CONFIRMED at binary scope).** `interactive.rs`'s
  3,116-line module and ~1,150-line `run` method are the largest
  instance of the T1/T3 theme; the key dispatch and overlay controller
  are the seams begging to be extracted.
- **Dead / aspirational declared surface (CONFIRMED, recurs).** Empty
  `keys.rs` (mirrors A2's empty `persistence.rs`), a fully-implemented
  `ToolExecutionUpdate` handler for an event AG1 confirmed is never
  emitted, plus no-op `TurnEnd`/`QueueUpdate` arms. Same shape as AG1's
  never-emitted variants and A2's vestigial module.
- **Truncation-looks-like-success (propagates to the UI, not
  re-introduced).** The pump faithfully renders the authoritative
  `MessageEnd` and the loop treats `Ok(Ok(()))` as silent success, so a
  truncated turn the runtime accepts as `Done` displays as a complete
  assistant message with no marker. The defect is upstream (M3/M4/AG1);
  the UI carries no guard of its own and no test pins the behavior.
- **Concurrency soundness (mostly sound, one real defect).** The
  select/channel skeleton is correct (unbounded channel, no dropped
  events, cancel token threaded), but holding the agent `TokioMutex`
  across the whole turn turns any mid-turn agent-locking selector into a
  full-loop freeze that defeats Ctrl+C — a lost-cancellation bug
  synthesis should weigh against AG1's clean cancel design.
- **Keybinding doc-vs-reality mismatch (CONFIRMED, T3).** `keys.rs`'s
  doc promises "double-Esc cancel" and "`Ctrl+O` to expand sub-agent
  transcripts"; neither exists (`Ctrl+O` opens the palette).
- **Chronology / roadmap-doc references in comments (CONFIRMED,
  recurs).** "Earlier versions of this method", "step 6.8 will port",
  "lands in a follow-up", "the legacy binary", and `docs/aj-next-*` §
  citations — same theme as AG1/SE1/A1/A2.
- **Happy-path-only coverage (CONFIRMED).** Leaf helpers are
  well-tested; the `run` loop — cancellation, mid-turn selectors,
  session swap/new, palette back-stack — is untested despite an
  available scripted + virtual-terminal seam.
