# Audit remediation — working backlog

Companion to `docs/audit/audit-plan.md` (the audit) and
`docs/audit/findings/` (the 23 reports + `_SUMMARY.md`). This file turns
those findings into a **priority-ordered, human-in-the-loop backlog** for
fixing.

The driving loop, in any session, is:

> work on the next item in `@docs/audit/remediation.md`

## How to work the next item

The main agent does this work **directly** — read the real code and write
the change yourself. Sub-agents are only for scouting (finding where
something lives / how it works), never for the proposal or the
implementation.

1. Read this file. Read the cited findings report(s) for the item and, if
   useful, `docs/audit/findings/_SUMMARY.md` for the cross-cutting context.
2. Pick the **topmost item not in a terminal state** (`DONE`,
   `ACCEPTED-AS-IS`, `DECLINED`). Document order *is* priority order.
   - If that item is `PROPOSED` (a proposal is already awaiting the user),
     do **not** start a new item — re-surface the pending proposal so the
     user can decide. Treat the user's reply as the decision.
   - If an item is blocked by an unfinished dependency, say so and either
     pick the next unblocked item or surface the dependency for a call.
3. **Gather context, then branch:**
   - If the right fix is genuinely unclear, or there's a real design fork,
     or it needs a product/architecture call — **ask the user questions
     first** and stop. Don't guess.
   - Otherwise, write a **proposal** (template below), present it, set the
     item to `PROPOSED`, and **stop for approval**.
4. **On approval**, implement it:
   - Keep the change minimal and consistent with the architecture. Fix at
     the right level — if the finding is one instance of a pattern, address
     the pattern (a shared seam), don't patch the one site and leave the
     siblings to drift. If doing so would balloon scope, say so and propose
     a staged plan rather than silently narrowing.
   - Run `cargo fmt`, `cargo check`, and the tests for the touched code
     (and `cargo clippy --workspace --all-targets` if lint-relevant).
   - Update this file: set status to `DONE`, add a one-line decision-log
     entry with the commit. Commit with a `<scope>: ...` message.
   - Stop. One item per invocation; do not chain.
5. **Recording a finding as okay** is a valid outcome. If the conclusion is
   "leave the code as-is" (optionally with a clarifying doc/comment change
   so the next reader doesn't re-flag it), set `ACCEPTED-AS-IS`, record the
   rationale in the decision log, make any doc/comment tweak, and commit.
   Use `DECLINED` for "acknowledged but deliberately not doing it now"
   (defer/out-of-scope), also with a rationale.
6. Always end a session by stating the item's new status and naming what
   the next item would be.

## Proposal template

When presenting a proposal, cover:

- **Problem** — what's wrong, with `path:line` evidence from the code (not
  just the finding).
- **Proposed change** — the concrete approach.
- **Architecture fit** — where it lives in the crate graph, which seam it
  uses or introduces, and whether it generalizes the fix across sibling
  call sites rather than patching one. Call out any boundary it touches.
- **Alternatives considered** — and why rejected.
- **Scope & risk** — blast radius, behavior changes, migration of on-disk
  data or public API, anything that needs a heads-up.
- **Test plan** — what proves it works; new tests at the boundary.
- **Effort** — S / M / L.

## Status legend

- `TODO` — not started.
- `PROPOSED` — proposal presented, awaiting the user's decision.
- `APPROVED` — user approved; ready to implement (or mid-implementation).
- `DONE` — implemented, verified, committed.
- `ACCEPTED-AS-IS` — reviewed; deliberately no behavioral change (rationale
  logged; may include a doc/comment clarification).
- `DECLINED` — acknowledged, not doing it now (rationale logged).
- `BLOCKED` — waiting on a dependency or a user decision.

---

## P0 — user-facing correctness / availability bugs

### R1 — Treat "stream ended without a terminal event" as a retryable error  [bug · DONE]
- **Sources:** M3 (Major), M4 (Major, all four providers), AG1 (Major), A3, A4; `_SUMMARY` P0 #1.
- **Problem:** when a provider stream closes without a terminal frame, all
  four adapters finalize the turn as a clean `Done`/`Stop`; the runtime
  trusts it and the UI shows it complete. Truncated turns are persisted and
  displayed as finished, and never retried.
- **Architecture angle:** fix at the `Provider` terminal contract + one
  runtime guard, not per-adapter band-aids or a UI patch. The four adapters
  share the `select_cancel`/finalize shape, so the correct fix is a single
  shared classification. Mind R7 (the `aj-agent` boundary) for the
  runtime-side guard. Pairs with R16 (test fixtures).

### R2 — Don't hold the agent mutex across a whole turn  [bug · DONE]
- **Sources:** A3 (Major); `_SUMMARY` P0 #2.
- **Problem:** `interactive.rs:1163` holds `agent.lock().await` across the
  entire `prompt().await`; other paths (`:1895`) lock the same mutex.
  Opening `/thinking` or `/model` mid-turn suspends the whole `select!`
  loop — including the cancel arm — until the turn ends (hang + lost
  Ctrl-C).
- **Architecture angle:** rethink what actually needs the lock vs. what can
  read a snapshot/handle. The fix should make the concurrency contract of
  the interactive loop explicit, not just shrink one critical section.

## P1 — serious bugs / data safety

### R3 — Bound markdown parsing depth  [bug · DONE]
- **Sources:** T4 (Major), A4; `_SUMMARY` P1.
- **Problem:** `render_list`/`parse_inline` recurse over nested
  `**`/`>`/`[[…]]` with only a width cap; assistant/user components feed
  untrusted model text in. Deep nesting can overflow the stack and abort
  the process.
- **Architecture angle:** a depth cap (graceful degradation to literal
  text past the limit) inside the markdown component, plus an adversarial
  regression test. Self-contained to `aj-tui`.

### R4 — Stop OAuth token bodies leaking into error strings  [bug · TODO]
- **Sources:** M5 (Major); `_SUMMARY` P1.
- **Problem:** a 2xx token response that fails to deserialize folds the raw
  body (live tokens) into `OAuthError::Parse` (`oauth/anthropic.rs:652`,
  `oauth/openai.rs:689`) → logs/terminal.
- **Architecture angle:** the secrets-redaction policy should be one shared
  helper used by both providers (ties to R10's OAuth de-dup); don't fix one
  provider only.

### R5 — Make user-data writes atomic / durable  [bug · TODO]
- **Sources:** C1 (Major), TO2 (Major), SE1 (Major); `_SUMMARY` P1.
- **Problem:** `Config::save` and the file-edit tools `fs::write`
  truncate-in-place (crash mid-write corrupts the file);
  `edit_file_multi` even advertises an "atomic" contract it doesn't honor;
  the session log never fsyncs despite documenting durability.
- **Architecture angle:** establish one durable-write approach
  (write-temp + rename for rewrites, explicit flush/fsync for the append
  log) — the `aj-models::auth` writer is the in-tree model — and apply it
  at every locus. Decide where that helper lives so it's shared, not
  copied.

### R6 — Guard the session log against concurrent writers  [bug · TODO]
- **Sources:** SE1 (Major); `_SUMMARY` P1.
- **Problem:** two `aj continue <id>` processes interleave JSONL lines and
  mint colliding entry ids, corrupting the parent chain. No file lock.
- **Architecture angle:** decide the concurrency contract for a thread file
  (single-writer lock vs. detect-and-refuse) at the `aj-session` boundary;
  the binary is where the second process is spawned.

## ARCH — architecture deviations (resolve early; other work builds on these)

### R7 — Resolve the `aj-agent → aj-conf` edge and the edition pin  [decision + change · TODO]
- **Sources:** AG1 (Critical); `_SUMMARY` dependency-graph section.
- **Problem:** `aj-agent` depends on `aj-conf` (`AgentEnv`,
  `ConfigThinkingLevel`), contradicting "the runtime depends only on
  `aj-models`." Also `aj-agent` is the only crate on `edition = "2021"`.
- **Architecture angle:** a real decision — move the shared types to a
  layer `aj-agent` may use (into `aj-agent`/`aj-models`, or a new tiny
  shared crate), or update the documented graph to admit the edge. This is
  a likely **ask-questions-first** item. The edition fix is mechanical once
  decided.

## STRUCT — high-leverage structural refactors (each retires many findings)

### R8 — Introduce a composition root for session setup  [refactor · TODO]
- **Sources:** A2 (Major), A3 (Major), A1 (Major), TO1; `_SUMMARY` theme 1.
- **Problem:** the session-setup pipeline is duplicated ~120 lines between
  print and interactive mode and four times within `interactive.rs`,
  already drifting; the CLI>env>config precedence overlay (5 sites) and the
  disabled-tools filter (3 sites) are sub-cases.
- **Architecture angle:** one `SessionSetup`/startup-inputs seam that both
  modes call. This is the single biggest finding-count win; do it as a real
  abstraction, not a shared free function grab-bag.

### R9 — Share one filterable-overlay-list for the selectors  [refactor · TODO]
- **Sources:** A4 (Major), T4 (Major); `_SUMMARY` theme 2.
- **Problem:** model/session/command-palette/prompt-history hand-copy a
  filterable-list + background-scan skeleton, while thinking/auth use the
  cleaner `SelectList` callbacks for the same job; inside `aj-tui`,
  `SelectList` and `SettingsList` are drifted twins.
- **Architecture angle:** converge on one list/overlay abstraction in
  `aj-tui` and have all binary selectors use it via callbacks.

### R10 — De-duplicate sibling provider code  [refactor · TODO]
- **Sources:** M5 (Major), M4 (Major); `_SUMMARY` theme 2.
- **Problem:** anthropic/openai OAuth modules are ~80% identical; OpenAI
  `classify_client_error` is copied three times.
- **Architecture angle:** extract the shared OAuth callback/parse/redaction
  seam and a single error-classifier; keep only the genuinely
  provider-specific bits per adapter. Absorbs R4's redaction fix.

### R11 — Standardize library error handling (no `anyhow` in lib crates)  [refactor · TODO]
- **Sources:** S1, S2, M1, AG2, SE1; `_SUMMARY` theme 3.
- **Problem:** `anyhow` reaches into `aj-models::refresh`, the `aj-agent`
  public tool trait (so every tool inherits it), `aj-session::repair`, and
  the SDK non-streaming methods (which also diverge from their streaming
  siblings' structured errors).
- **Architecture angle:** structured `thiserror` errors at every library
  boundary; `anyhow` only in the `aj` top level. The tool-trait error type
  is the highest-impact and most delicate (touches all tools) — may warrant
  its own staged proposal.

### R12 — Extract a shared test-support crate  [refactor · TODO]
- **Sources:** M5, TO1, T5, A5; `_SUMMARY` themes 5 & 7.
- **Problem:** the `aj-tui` `VirtualTerminal` harness is trapped in
  `tests/support`; the binary re-rolls a no-op `StubTerminal`; test doubles
  (`aj-models::scripted` with a `panic!` arm, `aj-tools::testing`) ship in
  the production public API.
- **Architecture angle:** a dev-dependency test-support crate (e.g.
  `aj-tui-testkit`) that owns the virtual terminal and shared doubles;
  moves the doubles out of shipped APIs and unblocks R17. Decide the crate
  boundary carefully so it doesn't pull prod code into a test crate.

## WIRING — finish or remove half-wired / dead surface

### R13 — Resolve `emit_update` / `ToolExecutionUpdate` end to end  [decision + change · TODO]
- **Sources:** AG1, AG2, TO2, A3, A4; `_SUMMARY` theme 4.
- **Problem:** the feature is wired across three layers but the runtime
  impl is a permanent no-op, leaving a dead event variant, bash
  self-throttling, and a fully implemented but never-triggered UI handler.
- **Architecture angle:** decide whether streaming tool-output updates are
  a feature we want. If yes, wire the runtime emit; if no, remove all three
  layers. Either way it's one coherent change, not three.

### R14 — Remove dead declared surface  [cleanup · TODO]
- **Sources:** AG1, T1, A1, A2, A3; `_SUMMARY` theme 4.
- **Problem:** never-emitted events (`TurnEnd`/`QueueUpdate`), always-empty
  `AgentEnd.messages`, six dead `Terminal` trait methods, empty doc-only
  `persistence.rs`/`keys.rs`, dead config schema (`ThinkingMinimal`).
- **Architecture angle:** delete per the YAGNI direction; each is small and
  local. Confirm no external/test consumer first (T1's dead methods are
  exercised only by test doubles).

### R15 — Make `@file` expansion real or remove the contract  [decision + change · TODO]
- **Sources:** A1 (Major), A2; `_SUMMARY` theme 4.
- **Problem:** `cli/file_args.rs::expand` returns input unchanged, yet
  `CLAUDE.md` + four code docs advertise `@path` expansion, and it's only
  wired into print mode.
- **Architecture angle:** implement it once (shared between print and
  interactive) or remove the feature and its docs. Likely a quick user
  call on which way to go.

## TEST — close key coverage gaps

### R16 — Add truncation / error-frame fixtures to provider roundtrips  [test · TODO]
- **Sources:** M3, M4; `_SUMMARY` theme 7. Pairs with R1.
- **Problem:** roundtrip suites are happy-path only; the R1 truncation bug
  lives in a path no fixture touches.

### R17 — Test the interactive `run` loop and a print full-turn  [test · TODO]
- **Sources:** A3, A5; `_SUMMARY` theme 7. Depends on R12.
- **Problem:** the ~1150-line interactive `run` loop has no test; the one
  integration test never enters `run`.

## SWEEP — mechanical, low-risk, batchable

### R18 — Remove comment chronology  [cleanup · TODO]
- **Sources:** every report; `_SUMMARY` theme 8. Strip "previously / used
  to / replaces the old / `docs/aj-next-plan §N` / future PR" framing and
  fix the few now-wrong comments. Batch per crate.

### R19 — Dependency hygiene  [cleanup · TODO]
- **Sources:** S2, M1, T1; `_SUMMARY` theme 9. Drop unused `async-stream`
  (openai-sdk, aj-models); move `aj-tui`'s direct version pins to
  `[workspace.dependencies]`.

### R20 — Reconcile truncation naming + hoist the tab-width constant  [cleanup · TODO]
- **Sources:** M2, TO1, T2, T4; `_SUMMARY` theme 9. Three correct-but-
  same-named `truncate` impls; "tab = 3 spaces" duplicated across three
  `aj-tui` files.

### R21 — Fix the keybinding cmd/meta parse-vs-format mismatch  [bug · TODO]
- **Sources:** T3; `_SUMMARY` theme 9. `keybindings` formats `cmd`/`meta`
  but `keys::parse_key_id` rejects them, so a configured `cmd+k` never
  fires.

## RESIDUAL — per-crate leftover Minor/Nit cleanup  [TODO]

Lowest priority. The themes above absorb most findings; what remains are
localized Minors/Nits in each report not covered elsewhere. When reached,
work one crate at a time, reading that crate's findings report and
batching the leftovers into a single reviewed change (or `ACCEPTED-AS-IS`
entries). Reports: anthropic-sdk, openai-sdk, aj-models-{core,streaming,
anthropic,openai,auth}, aj-conf, aj-agent-{runtime,contracts}, aj-session,
aj-tools-{framework,impls}, aj-tui-{core,text,editor,components,tests},
aj-{cli,core,interactive,components,tests}.

---

## Decision log

One line per resolved item (most recent last): `<date> · <id> · <status> ·
<commit> · <one-line note/rationale>`.

- 2026-06-04 · R1 · DONE · b09665d · Added
  `AssistantMessageEvent::truncated` (shared §10.3 classification) + a
  uniform `saw_terminal`/`finalize_or_truncate` seam on all four provider
  `StreamState`s, so a stream that ends before its terminal frame yields a
  retryable `Transient` error instead of a bogus `Done`; widened the
  agent retry gate from `Overloaded` to `Overloaded | Transient` (full
  §10.4 `RateLimit`/`retry_after_ms` policy deferred). Regression tests at
  each provider + the agent retry path.
- 2026-06-04 · R2 · DONE · 409a91e · Introduced a loop-side
  `RunConfigSnapshot` (provider/model/stream-options/thinking +
  `/model` pre-select key) as the source of truth for the next turn's
  config. The `/model` and `/thinking` selectors now read/write the
  snapshot without locking the agent; the footer renders from it; the
  submit handler copies it into the agent just before each turn under
  the turn's own (uncontended) lock — so a mid-turn model/thinking
  change is accepted immediately (footer wart accepted) but applies on
  the next turn. Session-changing commands (`/resume`, `/new`) reseed
  the transcript and stay agent-locking, so they're refused mid-turn
  with a notice. Net: no `agent.lock().await` is reachable from the
  select loop while a turn is in flight, so the freeze + lost-Ctrl+C
  bug is gone. Chose this `/tmp/pi`-style snapshot approach over the
  cheaper "gate all overlays" fix (keeps the palette usable mid-turn)
  and over a deeper `aj-agent` live-config refactor (R7-adjacent).
  Fixed the misleading `/quit`-mid-turn + `disable_submit` comments;
  unit-tested the busy notice; full run-loop test deferred to R17.
- 2026-06-04 · R3 · DONE · 5565fa8 · Threaded a nesting-depth
  counter through the markdown parser (`MAX_NESTING_DEPTH = 64`):
  `parse_markdown`/`parse_list` share one block-nesting counter
  (blockquote recursion + nested-list recursion), `parse_inline` an
  independent inline counter (emphasis/links). Past the cap the parser
  degrades to literal text instead of recursing, so untrusted model
  output can no longer overflow the stack (an uncatchable abort in
  Rust). The renderer needs no separate guard: `Block`/`Inline` are
  only built by the parser, so capping it bounds the AST depth the
  render recursion walks. Confirmed the practical overflow vectors are
  blockquotes and nested lists (emphasis/links don't deeply nest under
  the current word-boundary/first-match rules); capped inline anyway as
  cheap, uniform defense. Tests: an in-module parser-cap assertion on
  100k-deep input + integration tests that adversarial input renders
  without aborting and that within-cap nesting isn't clipped. Surveyed
  `/tmp/pi`: it delegates parsing to `marked` and survives only because
  a JS stack overflow is a catchable `RangeError` caught by a global
  handler — no technique to borrow; a pull-parser swap
  (`pulldown-cmark`) is the larger structural option, deferred. Noted
  a separate pre-existing O(n²) blowup on bare `[` runs (not a
  recursion bug) for later.
