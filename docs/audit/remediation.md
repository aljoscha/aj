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

### R4 — Stop OAuth token bodies leaking into error strings  [bug · DONE]
- **Sources:** M5 (Major); `_SUMMARY` P1.
- **Problem:** a 2xx token response that fails to deserialize folds the raw
  body (live tokens) into `OAuthError::Parse` (`oauth/anthropic.rs:652`,
  `oauth/openai.rs:689`) → logs/terminal.
- **Architecture angle:** the secrets-redaction policy should be one shared
  helper used by both providers (ties to R10's OAuth de-dup); don't fix one
  provider only.

### R5 — Make user-data writes atomic / durable  [bug · DONE]
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

### R6 — Guard the session log against concurrent writers  [bug · DONE]
- **Sources:** SE1 (Major); `_SUMMARY` P1.
- **Problem:** two `aj continue <id>` processes interleave JSONL lines and
  mint colliding entry ids, corrupting the parent chain. No file lock.
- **Architecture angle:** decide the concurrency contract for a thread file
  (single-writer lock vs. detect-and-refuse) at the `aj-session` boundary;
  the binary is where the second process is spawned.

## ARCH — architecture deviations (resolve early; other work builds on these)

### R7 — Resolve the `aj-agent → aj-conf` edge and the edition pin  [decision + change · DONE]
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

### R8 — Introduce a composition root for session setup  [refactor · DONE]
- **Sources:** A2 (Major), A3 (Major), A1 (Major), TO1; `_SUMMARY` theme 1.
- **Problem:** the session-setup pipeline is duplicated ~120 lines between
  print and interactive mode and four times within `interactive.rs`,
  already drifting; the CLI>env>config precedence overlay (5 sites) and the
  disabled-tools filter (3 sites) are sub-cases.
- **Architecture angle:** one `SessionSetup`/startup-inputs seam that both
  modes call. This is the single biggest finding-count win; do it as a real
  abstraction, not a shared free function grab-bag.

### R9 — Share one filterable-overlay-list for the selectors  [refactor · DONE]
- **Sources:** A4 (Major), T4 (Major); `_SUMMARY` theme 2.
- **Problem:** model/session/command-palette/prompt-history hand-copy a
  filterable-list + background-scan skeleton, while thinking/auth use the
  cleaner `SelectList` callbacks for the same job; inside `aj-tui`,
  `SelectList` and `SettingsList` are drifted twins.
- **Architecture angle:** converge on one list/overlay abstraction in
  `aj-tui` and have all binary selectors use it via callbacks.

### R10 — De-duplicate sibling provider code  [refactor · DONE]
- **Sources:** M5 (Major), M4 (Major); `_SUMMARY` theme 2.
- **Problem:** anthropic/openai OAuth modules are ~80% identical; OpenAI
  `classify_client_error` is copied three times.
- **Architecture angle:** extract the shared OAuth callback/parse/redaction
  seam and a single error-classifier; keep only the genuinely
  provider-specific bits per adapter. Absorbs R4's redaction fix.

### R11 — Standardize library error handling (no `anyhow` in lib crates)  [refactor · DONE]
- **Sources:** S1, S2, M1, AG2, SE1; `_SUMMARY` theme 3.
- **Problem:** `anyhow` reaches into `aj-models::refresh`, the `aj-agent`
  public tool trait (so every tool inherits it), `aj-session::repair`, and
  the SDK non-streaming methods (which also diverge from their streaming
  siblings' structured errors).
- **Architecture angle:** structured `thiserror` errors at every library
  boundary; `anyhow` only in the `aj` top level. The tool-trait error type
  is the highest-impact and most delicate (touches all tools) — may warrant
  its own staged proposal.

### R12 — Extract a shared test-support crate  [refactor · DONE]
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

### R13 — Resolve `emit_update` / `ToolExecutionUpdate` end to end  [decision + change · DONE]
- **Sources:** AG1, AG2, TO2, A3, A4; `_SUMMARY` theme 4.
- **Problem:** the feature is wired across three layers but the runtime
  impl is a permanent no-op, leaving a dead event variant, bash
  self-throttling, and a fully implemented but never-triggered UI handler.
- **Architecture angle:** decide whether streaming tool-output updates are
  a feature we want. If yes, wire the runtime emit; if no, remove all three
  layers. Either way it's one coherent change, not three.

### R14 — Remove dead declared surface  [cleanup · DONE]
- **Sources:** AG1, T1, A1, A2, A3; `_SUMMARY` theme 4.
- **Problem:** never-emitted events (`TurnEnd`/`QueueUpdate`), always-empty
  `AgentEnd.messages`, six dead `Terminal` trait methods, empty doc-only
  `persistence.rs`/`keys.rs`, dead config schema (`ThinkingMinimal`).
- **Architecture angle:** delete per the YAGNI direction; each is small and
  local. Confirm no external/test consumer first (T1's dead methods are
  exercised only by test doubles).

### R15 — Make `@file` expansion real or remove the contract  [decision + change · DONE]
- **Sources:** A1 (Major), A2; `_SUMMARY` theme 4.
- **Problem:** `cli/file_args.rs::expand` returns input unchanged, yet
  `CLAUDE.md` + four code docs advertise `@path` expansion, and it's only
  wired into print mode.
- **Architecture angle:** implement it once (shared between print and
  interactive) or remove the feature and its docs. Likely a quick user
  call on which way to go.

## TEST — close key coverage gaps

### R16 — Add truncation / error-frame fixtures to provider roundtrips  [test · DONE]
- **Sources:** M3, M4; `_SUMMARY` theme 7. Pairs with R1.
- **Problem:** roundtrip suites are happy-path only; the R1 truncation bug
  lives in a path no fixture touches.

### R17 — Test the interactive `run` loop and a print full-turn  [test · DONE]
- **Sources:** A3, A5; `_SUMMARY` theme 7. Depends on R12.
- **Problem:** the ~1150-line interactive `run` loop has no test; the one
  integration test never enters `run`.

## SWEEP — mechanical, low-risk, batchable

### R18 — Remove comment chronology  [cleanup · DONE]
- **Sources:** every report; `_SUMMARY` theme 8. Strip "previously / used
  to / replaces the old / `docs/aj-next-plan §N` / future PR" framing and
  fix the few now-wrong comments. Batch per crate.

### R19 — Dependency hygiene  [cleanup · DONE]
- **Sources:** S2, M1, T1; `_SUMMARY` theme 9. Drop unused `async-stream`
  (openai-sdk, aj-models); move `aj-tui`'s direct version pins to
  `[workspace.dependencies]`.

### R20 — Reconcile truncation naming + hoist the tab-width constant  [cleanup · DONE]
- **Sources:** M2, TO1, T2, T4; `_SUMMARY` theme 9. Three correct-but-
  same-named `truncate` impls; "tab = 3 spaces" duplicated across three
  `aj-tui` files.

### R21 — Fix the keybinding cmd/meta parse-vs-format mismatch  [bug · DONE]
- **Sources:** T3; `_SUMMARY` theme 9. `keybindings` formats `cmd`/`meta`
  but `keys::parse_key_id` rejects them, so a configured `cmd+k` never
  fires.

## RESIDUAL — per-crate leftover Minor/Nit cleanup  [IN PROGRESS]

Lowest priority. The themes above absorb most findings; what remains are
localized Minors/Nits in each report not covered elsewhere. When reached,
work one crate at a time, reading that crate's findings report and
batching the leftovers into a single reviewed change (or `ACCEPTED-AS-IS`
entries). Reports: anthropic-sdk, openai-sdk, aj-models-{core,streaming,
anthropic,openai,auth}, aj-conf, aj-agent-{runtime,contracts}, aj-session,
aj-tools-{framework,impls}, aj-tui-{core,text,editor,components,tests},
aj-{cli,core,interactive,components,tests}.

Per-crate progress (work top-to-bottom):

- anthropic-sdk — `DONE` (kept all public surface per the user, documented
  it as intentional; applied the 4 doc/attr nits).
- openai-sdk — `DONE` (kept all public surface per the anthropic-sdk
  precedent; finished the SSE-parse de-dup R11 left half-done; applied the
  contract doc + 2 nits).
- aj-models-core — `DONE` (collapsed the `tools::Tool` intermediary onto
  `types::ToolDefinition` per the reference's two-type design; applied the
  2 doc-link + retry-after-contract Minors and the provider chronology
  Nits; command-name Nit already consistent in-crate).
- aj-models-streaming — `DONE` (moved `select_cancel`/`SelectOutcome` to a
  `pub(crate)` `cancel` module, tightened `partial_json` to `pub(crate)` /
  private; dropped `repair_json`'s per-delta `Vec<char>`; doc + tests for
  the partial-number collapse and the `openai-responses` demotion; trimmed
  the duplicated strategy-chain comments. R20/R19 retirements verified).
- aj-models-anthropic — `DONE` (Major + Testing findings verify-only,
  retired by R1/R16; the boundary finding resolved with a `test-support`
  cargo feature applied across all four providers, which also retires the
  round-trip-helper portion of the `aj-models-openai` report's equivalent
  pub-for-tests finding (its `TextSignatureV1` envelope sub-item, a prod
  struct, stays for that sweep); plus the anthropic-local
  index/comment/simplicity cleanups).
- aj-models-openai — `DONE` (both Majors + the Testing/round-trip-helper
  Minors verify-only, retired by R1/R10/R16; reconciled the Codex
  terminal-error divergence to preserve the partial, removed the dead
  `ProcessOutcome.terminal`, kept+documented `TextSignatureV1`, added the
  tool-call id fallback, and applied the comment/clock/no-op cleanups).
- aj-models-auth — `DONE` (the two Majors + the scripted-surface
  Boundaries + the timeout/margin Contracts were verify-only, retired by
  R4/R10/R12. One behavior change, finding 4(1): a stored OAuth
  credential under an unregistered provider id now resolves to `Ok(None)`
  instead of hard-erroring `UnknownProvider`, matching the reference and
  giving the host's "log in" path instead of a raw error. Cross-checked
  the resolution chain against the reference, which also returns the
  none-equivalent for unknown providers. Recorded that the reference
  deliberately checks env *after* stored creds, the inverse of ours, as a
  noted-not-changed divergence. Doc'd the env-OAuth-no-refresh contract on
  `get_api_key` and the `extra` flatten/required-field behavior. Fixed
  `generate_state`'s per-byte `format!`, softened the request-head cap
  doc, and finished the clock dedup into one `oauth::now_unix_ms`.
  ACCEPTED-AS-IS the `find_env_keys` doc, already corrected to "four
  providers". Added the refresh-failure, unknown-provider, and
  manual-paste-race tests the report flagged as missing).
- aj-conf — `DONE` (the two Majors were verify-only, retired by R5: the
  non-atomic `Config::save` became the lock + read-merge-write
  `persist_changed`, and the clobber-guard/missing-file tests exist. Cross-
  checked the live items against the reference, which splits the same
  concerns across `config.ts`/`settings-manager.ts`/`resource-loader.ts`,
  funnels `$HOME` through one `homedir()`/`getAgentDir()`, stores sessions
  flat instead of per-project, and injects `cwd`/`agentDir` into its
  resource loader for hermetic tests. Acted on each: split the 2704-line
  `lib.rs` into `schema`/`paths`/`env` modules (API-stable via crate-root
  re-exports + a multi-file `impl Config`, so `Config::get_*` and friends
  keep their paths); added an injectable `AgentEnv::discover` seam under the
  real-host `new` wrapper (Minor Testing); centralized the six scattered
  `$HOME` reads into one `paths::home_dir` (Minor Simplicity); documented
  the `path_to_dir_name` collision on both it and `get_sessions_dir_path`
  rather than changing the on-disk layout (Minor Contracts, reference
  sidesteps it via flat storage); reworded the `Config` precedence doc to
  say the merge lives in `aj::model` (Minor Contracts) and de-duplicated the
  `ConfigThinkingDisplay` doc's doubled opening (Nit); fixed the `\u2014`
  escape (Nit); dropped the unused chrono `serde` feature. ACCEPTED-AS-IS
  the enum `Display`/`FromStr`/serde triple (now four enums): the TS idiom
  doesn't transfer and the drift tests compensate. Three pre-existing
  private-intra-doc-link warnings were left as-is (out of finding scope).)
- aj-agent-runtime — `DONE` (nearly all findings retired by themed work:
  the Critical `aj-conf` edge + edition pin by R7, the Major truncated-
  `Done` by R1, the Major three-dead-event-variants by R13/R14, the Major
  `tools::Tool` round-trip by the aj-models-core sweep, the Minor `anyhow`
  by R11, the Minor vestigial `AgentEnd.messages` by R14, the Nit comment
  chronology by R18; the Nit `determine_thinking` doc/ordering is moot
  since the trigger-word mechanism was removed). Two live leftovers, both
  cross-checked against the reference. (1) [Minor][Contracts] the two
  reachable `expect`s on `assembled_system_prompt`: the reference makes
  its `systemPrompt` a non-optional string defaulting to `""` and every
  provider tolerates an empty system prompt, and our wire layer already
  behaves identically (anthropic returns `None` on empty, the OpenAI
  builders gate on `!is_empty`, codex substitutes its default), so rather
  than the proposal's typed-error boundary guard we adopted the
  reference's design: made `assembled_system_prompt` a plain `String`
  (default `""`), dropped both `expect`s, and pass `Some(prompt)` to the
  `Context`. An unseeded agent now degrades to a promptless turn instead
  of panicking. Accessor returns `&str`; `seed_session` overwrites on
  `Some`; the two test callers in `session.rs` lost their `.expect()`.
  Net code reduction, no public-signature or on-disk change. (2)
  [Nit][Testing] the cancellation tests raced wall-clock timers (the
  multi-tool partial-cancel branch was already covered by a test added
  since the audit, but it and the streaming-arm test both raced a
  background `sleep`). The reference triggers aborts explicitly and lets
  a signal-responsive stream react rather than racing a timer; mirrored
  that: the batch test fires the cancel from inside the first probe's
  `execute` (new `cancel_on_start` field) so the in-flight batch is
  cancelled with no timer, and the streaming-arm test uses
  `#[tokio::test(start_paused = true)]` so the 50ms cancel deterministically
  precedes the 60s Done (also fixing a real, if rare, Start-vs-cancel
  ordering race). New test pins the unseeded-agent no-panic behavior. A
  fresh-agent review found no must-fix bugs and confirmed `Some("")` is
  handled identically to absent across all four providers; its should-fix
  doc-accuracy items (the codex "sends no system prompt" overstatement, a
  pre-existing semicolon, an awkward `AgentSeed` doc) were folded in.
  Flagged, not done: provider-level empty-prompt tests (anthropic/openai
  lack an explicit `Some("") → no system block` test now that the path is
  reachable) left for a possible aj-models follow-up to keep this change
  aj-agent-scoped. The `SessionState`-publicness boundary note is punted
  to the aj-agent-contracts (AG2) sweep. `cargo test -p aj-agent` (77) +
  `-p aj` session tests green, `fmt`/`clippy -p aj-agent -p aj
  --all-targets` clean, `cargo check --workspace` confirms no drift.

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
- 2026-06-09 · R4 · DONE · 29d8e2f · Added a shared `pub(crate)`
  `oauth::redacted_body_summary` helper and routed both providers'
  token-endpoint `Parse` arms through it, so a 2xx token body that
  fails to deserialize no longer folds live `access`/`refresh` tokens
  into `OAuthError` (whose `Display` reaches logs/stdout). Kept the
  serde error in the message — it names the drifted field without
  echoing its value, the realistic trigger — and replaced the raw body
  with a byte-length summary. Left the non-2xx `Server { body }` arm
  unredacted (per the proposal): it carries RFC-6749 error objects, not
  tokens, and is the key login-failure diagnostic. Rejected the
  strip-named-fields alternative as unsafe for the exact trigger (a
  renamed/added token field wouldn't be in the strip-list). Put the
  helper in `oauth.rs`, the existing shared seam, so R10's OAuth de-dup
  folds it in. Regression test per provider asserts a token-bearing 2xx
  `Parse` error omits the token material.
- 2026-06-09 · R5 · DONE · 727d9ce · Reframed (with the user) from the
  proposed atomic-temp+rename/fsync sweep to a **lock + read-merge-write
  update primitive** for `config.toml`, after the reference
  implementation showed its only durability mechanism is a cross-process
  lock + merge-only-changed-fields (plain write, no fsync anywhere) and
  AJ's own `auth.json` already does exactly that. The real bug for
  config was *clobbering*, not crash-truncation: `Config::save` stamped
  the full stale in-memory snapshot with no lock. Replaced `save` with
  `Config::persist_changed(baseline)` — a synchronous `ConfigLock`
  (sidecar `.lock` dir, the sync twin of `auth.rs`'s) wraps a re-read of
  the file and applies only the options that differ from `baseline`, so
  a second `aj` editing a different key isn't overwritten; comments,
  unknown keys, the invalid-TOML clobber guard, and missing-file
  creation are preserved. `persist_config` applies the mutation to the
  live config first and persists off the mutex, keeping the
  best-effort-live-on-failure contract. The other two loci R5 bundled
  are **ACCEPTED-AS-IS**, matching the reference's plain-write/no-fsync
  behavior, with overstated docs corrected: `edit_file_multi`'s
  "atomic" now means all-or-nothing *validation* before one ordinary
  in-place write (TO2), and the session log's "durable as soon as the
  call returns" is now "written to the OS (survives a process crash),
  not fsync'd" (SE1 durability half; the session single-writer lock
  stays R6). Rejected the new `aj-fs` crate + temp+rename + fsync as
  over-engineered relative to the reference and the small files
  involved. Tests: clobber regression, invalid-TOML refusal, missing
  file, comment preservation, lock acquire/release + stale-steal.
- 2026-06-17 · R6 · DONE · 15a7018 · Reframed (with the user, after
  reading the reference) from the proposed single-writer lock to
  **collision-free random entry ids**, matching what the reference does:
  it takes no session-file lock at all (it locks auth/settings/trust but
  deliberately not sessions) and stays corruption-free under two
  concurrent writers purely by minting random ids. AJ's actual bug was
  its id *scheme*, a per-process `{:08}` counter seeded from the max id
  on disk, so two `aj continue <id>` processes mint identical ids and
  `entries.insert` overwrites, breaking the parent chain. Replaced
  `next_id`/`parse_id_counter`/`next_counter` with `mint_id` (a random
  `u32` as 8 hex digits, re-drawn on the astronomically rare
  within-process collision via the `entries` map) and switched the two
  per-line `writeln!`s to a single `write_all` of `format!("{json}\n")`
  each, so under `O_APPEND` concurrent appends interleave whole lines
  instead of tearing one. Backward compatible: ids are opaque strings,
  old decimal-id logs still load, and a new hex id that happens to equal
  an existing decimal id is caught by the mint check. Honestly documented
  the residual: a cross-process id collision is possible at ~1/2^32 (not
  "never"), and two concurrent resumers grow sibling branches off the
  shared head so on re-resume one writer's tail is left off the
  linearized path (accepted over a lock). Rejected an OS advisory lock
  (`flock`)/sidecar-lock: heaviest option, needs a Windows path, must be
  held for the whole multi-hour session (for which the auth/config
  mkdir+stale-mtime pattern doesn't fit), and diverges from the
  reference. Tests: two-resumer distinct-ids + clean-re-resume
  regression (would fail under the old counter) and within-log id
  uniqueness across many appends. Verified by a fresh-agent review.
- 2026-06-17 · R7 · DONE · f6ca82f · Decoupled the runtime from
  `aj-conf` (Option B, confirmed against the reference, whose
  `pi-agent-core` likewise depends only on its models layer and takes a
  finished system-prompt string). The runtime no longer assembles the
  prompt or holds an `AgentEnv`: `Agent::with_provider` now takes a
  plain `working_directory: PathBuf` (the only host environment the
  loop needs, for `SessionState`/tool cwd) and a wire-level
  `Option<ThinkingConfig>` instead of `Option<ConfigThinkingLevel>`;
  `assemble_system_prompt` and the `env()` getter are gone, and
  sub-agent spawn roots off `session_state.working_directory()`. The
  host now owns assembly: a new `aj::system_prompt::assemble_system_prompt`
  (in the binary, next to `SYSTEM_PROMPT`) builds the string from the
  `AgentEnv` plus a `read_file`-presence gate, and the
  `ConfigThinkingLevel → ThinkingConfig` map moved to `aj::model`
  (shared by both modes). The interactive `SessionWorld` keeps the
  `AgentEnv` it built so the context notice, footer cwd, and editor
  autocomplete read it directly instead of through `agent.env()`. Net:
  `aj-agent` depends only on `aj-models`, matching the documented
  graph; `aj-conf` stays a leaf. Also fixed the manifest drift the
  finding bundled in: `aj-agent` now inherits `edition`/`version` from
  the workspace (was pinned to edition 2021 / 0.1.0), which entails the
  expected style-edition-2024 reformat of `queue.rs`/`tool.rs` and some
  import reordering. The skills-listing gate test moved with the
  assembly into the binary; provider roundtrips and the agent event
  protocol are unaffected. Verified the whole workspace compiles +
  tests pass in an isolated worktree (the change landed alongside
  concurrent work, so it was built and tested in isolation).
- 2026-06-17 · R8 · DONE · baf6234 · Built the composition root
  around the interactive refactor that had already landed since the
  audit: R2/R7's `SessionWorld` retired A3's four-times-within-
  `interactive.rs` duplication, so the live R8 scope was the
  print↔interactive split (A2), the precedence merge (A1), and the
  disabled-tools filter (A1/TO1). Did full convergence (user call):
  extracted a new mode-agnostic `aj::session_setup` module (sibling of
  `turn`/`model`) owning `RunConfigSnapshot` + `build_run_config`,
  `build_initial_run_config` (the CLI>env>config model resolution +
  scripted/registry branch + `RestoreContext`), `build_agent`,
  `restore_session_settings`, and two new pure helpers `prepare_log`
  (log resolve + repair + re-linearize + restore) and `freeze_and_seed`
  (system-prompt freeze + initial settings record + transcript seed).
  Both `SessionWorld::build` and `print::run` now ride these identical
  primitives, so the restore logic that had drifted (print printed
  ad-hoc stderr lines and lacked the same-model-different-speed header
  rebuild; recorded the scripted model as `scripted/scripted` rather
  than the merged provider id) lives once. For A1, added
  `model::ModelSelection::{merge, provider_id}` as the single home for
  the overlay and changed `model::resolve` to take it (collapsing the
  4 hand-rolled `.or(config)` sites + 2 divergent provider-id-default
  idioms). For TO1/A1, added `aj_tools::builtin_tools(opts, disabled)`
  so the disabled-tools filter + its `tracing::info!` live behind the
  catalog seam, called by both `build_agent` and (nothing else now);
  `get_builtin_tools` stays for the unfiltered autocomplete consumer.
  Net -406 lines. Behavior change is intentional and small: print's
  resume-restore now emits the uniform notice text and gains the
  same-model-different-speed header rebuild, and print's model
  resolution becomes eager (the CLI/config model is resolved up front,
  matching what interactive already did) rather than lazy, so a dead
  configured-model pin no longer silently rescues itself off the
  resumed log's recorded model. Tests: `ModelSelection` precedence +
  default, `builtin_tools` filtering, `build_initial_run_config`
  scripted-path provider-id merge, and a same-model-different-speed
  resume that exercises the bundle-rebuild branch; the existing
  `SessionWorld` restore/seed suite now covers print's setup too since
  both call the same code. Full print `run`-loop test stays R17
  (depends on R12). `fmt`/`check`/`clippy` clean; `aj` + `aj-tools`
  suites green.
- 2026-06-18 · R9 · DONE · 2fc6b74 · Converged the selector overlays
  onto a single `aj-tui` `FilterableSelect` (search `TextInput` +
  `SelectList` + the cancel/confirm/navigate/type-to-filter routing +
  `on_select`/`on_cancel` + an `on_query` hook that defaults to
  `SelectList::set_filter`), retiring the four hand-copied
  search-overlay impls and the two divergent wiring patterns
  (manual-routing vs. callbacks). model keeps its per-field fuzzy
  scoring through `on_query` (+ new `SelectList::set_items`); the two
  streaming selectors share a generic `StreamingScan<T>` (background
  scan + coalesced drain + loading flag), and prompt-history's two
  scopes became a lazily-spawned second `StreamingScan` instead of a
  tagged channel. Read-only `auth_status`/`help_overlay` became
  `build_overlay` fns over a shared `ReadOnlyListOverlay`. Unified the
  three outcome-handle shapes into one `OutcomeSlot<T>` and applied it
  across all the picker/viewer overlays (the 7 A4-flagged ones plus
  `agent_picker`/`task_output`/`usage_status`) so a single shape
  remains; `settings_window`/`skills_window` keep their distinct
  submenu/changes protocol. Decision A (user call): kept `SelectList`
  and `SettingsList` separate (genuinely different semantics, the
  reference keeps two too) and only unified the nav-key set — gave
  `SettingsList` the page keys `SelectList` already had. Folded in the
  A4 minors: routed the session preview through the display-width
  authority (`ansi::truncate_to_width` + new `ansi::strip_ansi`, so a
  truncated current-session row no longer bleeds the selection
  highlight via an injected reset) and reworded the model rebuild
  comment to name the real reason (custom field scoring, not a
  `SelectList` limitation). `SessionSelectorOutcome::Confirmed` now
  carries the session id the host used, not the whole preview. A fresh
  sub-agent review caught two should-fixes, both addressed: the
  streaming current-row chase now yields on any navigation key (even a
  no-op boundary Up, which a before/after selection compare missed),
  and the preview-truncation reset-bleed (above). Net -624 lines.
  Tests: FilterableSelect routing/query/loading/status, StreamingScan
  drain, `strip_ansi`, the boundary-navigation chase yield, and every
  migrated selector's existing suite carried over; `fmt`/`clippy`
  clean, full `cargo test --workspace` green (85 binaries).
- 2026-06-19 · R10 · DONE · a25d599,99da18f · Two-part sibling-provider
  de-dup, no behavior change. (A) The three OpenAI adapters each carried
  the same `ClientError → AssistantError` fan-out (Completions/Responses
  byte-identical, Codex re-spelling all four arms to overlay a friendly
  429 message). Factored one `crate::openai::errors::classify_client_error`
  plus a `classify_client_error_with` that lets a caller rewrite a typed
  `ApiError`'s message (not its category) before classification; Codex's
  wrapper now passes its overlay through the latter. Put it at the
  adapter layer, not `crate::errors`, so that module stays free of any
  `openai-sdk` type (its helpers take decomposed primitives). The
  Anthropic provider's `classify_client_error` is a genuinely different
  classifier (different SDK, `classify_anthropic_error`) and stayed out.
  (B) The Anthropic/OpenAI OAuth modules were ~80% identical. Extracted
  three private `oauth` submodules — `callback` (the loopback listener
  loop + request-head reader + response writer + query parsing, behind a
  `CallbackConfig { path, provider_name }`; `await_callback` returns just
  the code since both providers already validate `state == expected_state`
  in the handler, so Anthropic's carried-out state was always
  `expected_state`), `paste` (`ParsedAuth` + `parse_authorization_input`),
  and `token` (`TokenResponse`, `send_token_request` as the single home
  for the non-2xx→`Server` and redacted 2xx-parse-failure handling,
  `now_unix_ms`, and the timeout/refresh-margin constants with a note
  relating the margin to the auth lock + request timeouts) — plus a
  test-only `test_support` with one `MockTokenServer` (the superset
  capturing body + `Content-Type`) shared by both provider suites. Each
  provider keeps only its endpoint constants, `build_authorize_url`,
  state policy, body encoding (JSON vs form), redirect tracking, and
  `token_to_credentials` (OpenAI's JWT account-id extraction). Net
  ~-437 lines; moved tests live with the code they exercise. Left the
  `auth.rs::current_unix_ms` comment nit for the RESIDUAL pass (separate
  module, outside this seam). `fmt`/`clippy` clean; `cargo test
  -p aj-models` + `cargo check --workspace` green.
- 2026-06-19 · R11 · DONE · 2593ea6 · Resolved (with the user) to a
  relaxed-but-principled stance after auditing every `anyhow` library
  site by the only question that matters: does a caller branch on the
  error or only render it. Typed `thiserror` errors where callers
  branch, a named opaque `Box<dyn Error + Send + Sync>` (new
  `aj_agent::BoxError`) where the cause is only rendered, and no
  `anyhow` in any public library signature. The SDKs' non-streaming
  `messages`/`chat_completions`/`responses` were *converted* (not
  removed, per the user: keep them for future use) to `ClientError`
  through a new shared `classify_error_response`/`retry_after_header`
  that the streaming paths now share too, retiring the copy-pasted
  non-2xx mapping; anthropic's `anyhow` became a dev-dependency for its
  example, openai's was dropped. `aj-models` gained a typed
  `RefreshError` (Fetch/Http/Parse/Write/NoCachePath). In `aj-agent`
  the tool trait (`execute`/`spawn_agent`/`ErasedToolFn`), the
  event-bus `Listener`/`emit`, and `TurnError`'s `Recoverable`/`Fatal`
  payloads now carry `BoxError` (`From<anyhow::Error>` became
  `From<BoxError>`); `run_single_turn`/`execute_tool` return it.
  `aj-session`'s `repair` returns `ConversationError` and the
  persistence listener returns `BoxError`. Every tool in `aj-tools`
  returns `BoxError`. `anyhow` is now confined to the `aj` binary,
  which bridges `TurnError` payloads via `anyhow::Error::msg` (not
  `Error::from`, which doesn't compile for a boxed trait object). The
  CLAUDE.md error-handling rule was rewritten to this stance. Rejected
  the stricter "rich `thiserror` enum everywhere" because the
  render-only seams have no caller that branches, so extra variants
  would be unused; rejected "just keep `anyhow`" because that leaks the
  dep into the public tool-author and bus surface. Tests: shared
  `classify_error_response` parity in both SDKs plus a
  `RefreshError::Parse` regression; full `cargo test --workspace` green
  (86 binaries), `fmt`/`clippy` clean. A fresh-agent review confirmed
  no behavioral regression on the live paths (streaming classification
  byte-equivalent, `oauth_usage` intentionally kept off the helper,
  overflow give-up rendering faithful) and caught a CLAUDE.md
  scope-creep slip (two unrelated lines dropped), now restored.
- 2026-06-19 · R12 · DONE · e5d8155 · Three-part test-support
  extraction, no runtime behavior change. (1) Promoted `aj-tui`'s
  `tests/support` harness into a new dev-dependency-only crate
  `aj-tui-testkit` (depends on `aj-tui` + `vt100-ctt`/`tokio`; the
  `aj-tui` → testkit edge is a dev-dep so Cargo permits the cycle).
  `support.rs` became the crate's `lib.rs`; the seven submodules moved
  verbatim with `super::` → `crate::` path fixes. The 54 integration
  tests switched `mod support;` → `use aj_tui_testkit as support;`, one
  uniform line each (five files that declared but never used `support`
  dropped the import). So the `VirtualTerminal` is now reusable across
  crates and no longer ships in `aj-tui`'s public API. (2) A5: deleted
  the binary's no-op `StubTerminal` and pointed `replay_parity` at the
  shared `VirtualTerminal` (testkit added as an `aj` dev-dep), so the
  parity test now drives the real compositor path; the full run-loop
  test that asserts on rendered frames stays R17, which this unblocks.
  (3) TO1: gated `aj-tools::testing` behind `#[cfg(any(test, feature =
  "testing"))]` + a new `testing` feature, removing `DummyToolContext`
  from production builds (nothing outside the crate's own `cfg(test)`
  used it). Left `tempfile` a normal dep — `tools::bash` needs it on
  the live spill-file path — and noted the deviation from the finding's
  "move tempfile to dev-deps" sub-point. (4) M5: kept `scripted` in
  prod (the `--scripted` flag legitimizes `ScriptedProvider`/`demos`/
  `ScriptBuilder`) and documented `ExhaustedBehavior::Panic` as the one
  reachable, opt-in, test-only `panic!` (default and `--scripted` both
  use `EndTurn`), an ACCEPT-the-module-as-prod call rather than a
  cfg-gated enum variant. Rejected one mega test-support crate
  (would invert layering by pulling `aj-models`/`aj-tools` into a TUI
  testkit); each double got the treatment matching its dependency
  reality. Also fixed the now-wrong `tests/support/` doc references
  across `aj-tui/src` (lib/terminal/components/example) to point at the
  testkit; left the README's chronology sections for R18. `fmt`/`clippy
  --workspace --all-targets` clean; `cargo build --workspace` confirms
  no double leaks into a default build; full `cargo test --workspace`
  green (88 binaries incl. the new crate).
- 2026-06-19 · R13 · DONE · 3450980 · The audit's premise (runtime emit
  is a permanent no-op) was stale: the "yes, this is a feature we want"
  branch had already been taken and shipped by `3450980` ("stream
  foreground tool progress to the bus"), so R13's decision + change were
  done before this session. Verified the path is coherent across every
  layer the finding named: the runtime `emit_update` builds and
  `bus.emit().await`s a real `ToolExecutionUpdate` inline (lib.rs:2827,
  best-effort, ordered before the terminal `ToolExecutionEnd`); the
  `ToolContext::emit_update` contract doc (tool.rs) and the `events.rs`
  variant doc describe the live inline-await emit, not a no-op; bash's
  foreground loop awaits `emit_update` so its self-throttle now feeds a
  real consumer (no longer dead work); the interactive `event_pump`
  dispatches it to `ToolExecutionComponent::update_partial` (live, not
  dead); and print mode deliberately skips it (high-frequency transient
  render data, not structured output). Tests confirm it end to end
  (`aj-agent::foreground_tool_progress_emits_update_before_end` asserting
  Start→Update→End ordering, `aj-tools::emit_update_fires_during_execution`),
  both green. No new code change in this session: the behavioral change
  happened in `3450980`, so DONE crediting that commit is the honest
  label over ACCEPTED-AS-IS. Left the residual `bash_execution.rs`
  comment chronology ("Today the formatting is text-only", `docs/
  aj-next-plan.md §1.2/§1.3` refs) for R18's per-crate sweep, and treated
  the TO2 "duplicated debounce constant" sub-note as a non-issue (both
  sites reference the single `UPDATE_DEBOUNCE` constant, can't drift; the
  "skip snapshot when no sink" optimization is moot now the production
  wrapper always consumes).
- 2026-06-19 · R14 · DONE · 8bd98b8,8250e29,3bedb89,0747a0b,58d8e87,c4b4081
  · Comparing each flagged item against the reference reframed the
  finding: most "dead surface" was not invented cruft but **unfinished
  ports** of real reference contract, so the call (with the user) was to
  *finish the ports*, not delete. Split into six commits. **Genuinely
  dead AJ-only scaffolding, deleted:** the empty doc-only `persistence.rs`
  / interactive `keys.rs` modules, the producer-less
  `CommandAction::NotYetImplemented`, and `Tui::should_render` +
  `last_render_time` (no caller, and keyed on the wrong interval). **Real
  reference features we'd declared but never wired, now finished:** (a)
  `TurnEnd` is the reference's `turn_end {message, toolResults}` and
  `AgentEnd.messages` its `agent_end {messages}` — both load-bearing
  there; we now emit `TurnEnd` once per turn (one inference + its tool
  batch, bracketed by a `TurnStart` moved into the loop and guarded by a
  `retrying` flag so a transient retry doesn't re-bracket) and populate
  `AgentEnd.messages` with the transcript clone; kept our extra
  `TurnUsage` as the lighter per-turn usage signal (not in the reference).
  (b) `Terminal::set_title` / `set_progress` are actively wired in the
  reference's interactive mode; we now set the window title in
  `SessionWorld::install` and light the OS progress indicator while the
  main agent runs (edge-gated in the event pump). **Kept + documented
  (interface completeness):** the `Terminal` move/clear verbs have no
  render-loop caller in the reference either (it inlines its hot-path
  escapes like we do), so they stay as the portability seam with a trait
  doc note. **Reintroduced `minimal`** thinking level end to end (the wire
  `ThinkingLevel` already had it and the `thinkingMinimal` theme token was
  orphaned): `ThinkingConfig`/`ConfigThinkingLevel::Minimal`, the name
  vocabulary, both projections, config↔model maps, the selector catalog,
  the settings-window schema, and the tint. **Recorded stale, not
  touched:** `QueueUpdate` (the audit's "no producer" was stale — it's
  live) and the print-mode no-op listener (already removed; only a stale
  comment, left for R18). A fresh-agent review confirmed `TurnEnd` fires
  exactly once per completed turn and never on retry/abort/error, no
  use-after-move, the `minimal` thread-through is complete, and
  fmt/clippy/full-workspace tests are green; its doc/style nits were
  folded back into the bucket commits. Tests: `TurnEnd` payload +
  `AgentEnd` snapshot, single-bracket-across-retry, the progress
  indicator following main busy/idle, and `minimal` round-trips.
- 2026-06-22 · R15 · DONE · ff18b0e · Like R13, the finding's premise was
  stale: the no-op `expand` passthrough had already been replaced with
  real argument-level resolution by `ff18b0e` ("resolve @file launch
  arguments into prompt content"). Verified the implementation matches the
  finding's first option (implement once, shared between both modes) rather
  than the remove-the-contract option. `cli::file_args::process_file_args`
  resolves each `@`-prefixed positional cwd-relative (with `~` expansion):
  text files become a `<file name="ABS">…</file>` block, images are resized
  under the inline budget and attached as real `UserContent::Image` blocks,
  a missing file is a hard error, empty files are skipped. The resolver is
  owned by `cli::initial_input` and called by **both** modes (`print.rs:114`,
  `interactive.rs:322`), retiring the print-only / interactive-never-called
  asymmetry the finding flagged. All four docs the finding named now
  describe the live behavior (`Args::prompt` and `Command::Continue` in
  `cli/args.rs:58,121`, the `cli` module doc, and `lib.rs`); `CLAUDE.md` no
  longer mentions `@file` at all. The editor's separate `@file` autocomplete
  (interactive typing, `editor_ext`) is a distinct real, tested feature, not
  a stale contract. No new code change this session: the behavioral change
  happened in `ff18b0e`, so DONE crediting that commit is the honest label
  over ACCEPTED-AS-IS. The lone residual is a historical progress-log line
  (`docs/aj-next-progress.md:792`, "passthrough stub for `@file`
  expansion") which correctly records a past development state and is left
  as-is. `cargo test -p aj --lib cli` green (24 tests).
- 2026-06-22 · R16 · DONE · 90a72f4 · Closed the happy-path-only gap the
  M3/M4 testing findings flagged (the path the R1 truncation bug hid in):
  the four roundtrip suites only replayed well-formed SSE ending in a
  terminal frame, so the §10.3 error legs were pinned only by hand-built
  in-module events, never against captured wire fixtures. Added 10 `.sse`
  fixtures + error/truncation scenarios across all four suites, replayed
  through the existing public `replay_sse_events` seam (same parse path
  the live provider runs, so they catch fixture/code drift like the happy
  path does). Each asserts the terminal classification (`stop_reason` +
  `error.category`) and that partial deltas survive: anthropic
  truncated→Transient / mid-stream `error` frame→Overloaded / refusal
  `stop_details`→ContentFilter; completions truncated→Transient /
  `finish_reason: content_filter`→ContentFilter; responses
  truncated→Transient / `response.incomplete(max_output_tokens)`→clean
  `Done(Length)` (the positive control that separates a real length stop
  from a transport drop) / `response.failed`→Error; codex
  truncated→Transient / top-level `error` frame→RateLimit. Gave error
  scenarios their own single "terminal classification" shape rather than
  forcing them through the parse/serialize/semantic trio (serialize +
  semantic-roundtrip are meaningless for a turn that's never sent
  upstream). Two honest behaviors pinned by comment rather than
  changed (R16 is test-only): the codex error path drops the accumulated
  partial (the M4 divergent-terminal-handling Minor), and a streamed
  `response.failed` carrying a bare `server_error` (no HTTP status on the
  SSE frame) lands in `Unknown`, not `Transient`. `cargo test -p aj-models
  --test roundtrip` green (60, +10); `fmt`/`check`/`clippy --tests` clean.
- 2026-06-22 · R17 · DONE · 4ea8dbd · Added the binary's first tests
  that *enter* its two most defect-prone seams: the interactive
  per-session select loop and the print turn driver. **Interactive
  (the Major):** a `run_loop_tests` module builds a real `Shell` +
  `SessionWorld` around a headless `aj-tui-testkit` `VirtualTerminal`
  and drives `run_session` to completion, asserting on the rendered
  chat scrollback (the `ChatView` slot, same read path as
  `replay_parity`) and the returned `SessionExit`. Determinism comes
  from `#[tokio::test(start_paused = true)]`: tokio auto-advances the
  clock only once every task is parked, i.e. once the loop has drained
  the turn's bus events and gone idle, so a feeder task whose `sleep`
  gates the quit key provably fires *after* the turn has fully rendered
  (no wall-clock waits, no biased-select race against the still-draining
  event arm). Three cases. (1) A full scripted turn auto-submits via
  `launch_content`, streams a reply through the loop into the chat, and
  an idle Ctrl+C quits (plus the turn round-tripped to disk). (2) A bare
  Ctrl+C quits when idle. (3) A mid-turn Ctrl+C (provider parked on a
  long delay) cancels the in-flight turn, renders "Turn cancelled.",
  drops the would-be reply, and the still-live loop accepts a second
  Ctrl+C to quit. Case (3) is the direct R2/A3 lost-cancellation
  regression.
  Chose `run_session` over `InteractiveMode::run` as the seam because
  `run` does process-global I/O (real `~/.aj`, `ProcessTerminal`) while
  `run_session` is where the consequential control flow lives and takes
  injected state. Session swap/new stays covered at the seam level
  (`build_next_world*` + selector-outcome tests), not re-driven through
  the brittle palette-navigation path. **Print (the Minor):** extracted
  a testable `run_inner` from `print::run` taking the config, auth
  store, persistence, cwd, and an `Arc<Mutex<W>>` output sink by value;
  `run` keeps the process-global resolution (`Config::load`,
  `AuthStorage::at_default_path`, sessions dir, cwd, stdout) and
  delegates. `json_event_listener`/`print_final_assistant_text` now
  write to the injected sink. Two `start_paused` tests drive the
  `streaming-text` scripted demo through `run_inner` with a captured
  buffer: text mode asserts the final assistant text and that the turn
  persisted (via a disk resume); json mode asserts one valid JSON object
  per line and the assistant text on the stream. The
  `ToolExecutionUpdate` filter is covered by its own unit test feeding
  synthetic events through `json_event_listener` (the `streaming-text`
  demo emits no tool updates, so asserting absence in the driven test
  would be vacuous). The one production behavior change is that the
  `run`-side dependency resolution (including `get_sessions_dir_path`'s
  directory creation) now runs before `run_inner`'s argument validation,
  so a no-prompt misuse opens the credential store and creates the
  sessions dir before erroring (happy path unaffected, documented on
  `run`). Tests run deterministically across repeated runs. `cargo test
  -p aj` green (444 lib + integration), `fmt`/`clippy --workspace
  --all-targets` clean. Review nits (vacuous filter assertion, in-memory
  persistence check, em-dash/semicolon comment style) addressed in a
  follow-up commit.
- 2026-06-22 · R18 · DONE · 61a240d · Stripped comment chronology
  workspace-wide, with the user broadening scope from just
  `docs/aj-next-plan.md` to **all** spec-doc mentions. Removed every
  `docs/*.md` citation in Rust comments (the completed `aj-next-plan.md`
  migration plan plus the live `models-spec`/`compaction-spec`/
  `openrouter-spec`/`subagent-observability` design specs, ~137 sites)
  and the ~337 bare section markers they left behind (`§7.4.3`, `§10.3`,
  ...), keeping the comment prose self-standing. RFC section refs
  (`RFC 7231 §7.1.3`) were deliberately kept. Also reworded the
  code-history chronology the audit named: "Migrated to ... per ...",
  "Replaces the old", "the legacy `aj` binary / renderer / convention"
  (kept "legacy logs"/"legacy on-disk shape"/"budget-based (legacy)
  models" as live data-compat domain terms), "Phase 0/1/2" migration
  framing, "no longer spawns/injects", "Today the formatting is
  text-only", "we used to prepend", "an earlier version of the editor",
  and fixed two now-wrong comments (the dead `[crate::bridge]` rustdoc
  link in `todo.rs`, and `aj-agent`'s stale "Today the agent never fires
  [cancellation]" doc, since cancellation is fully wired now). The bare
  `§` removal was driven by a reviewed regex script (preview + diff
  review), then the multi-line-reference and dangling-clause artifacts
  it left were hand-fixed. A fresh-agent review of the full diff caught
  27 mechanical-strip breakages (dangling "Per"/"as:"/"says", doubled
  em-dashes, leftover "/", lost sentence objects) plus polish and
  residual-chronology items, all addressed. Left the orphaned
  `docs/aj-next-plan.md` file in place (user call: the spec docs are
  vehicles for implementing changes, fine if unreferenced from code).
  Comment-only change, no behavior change: `fmt`/`check`/`clippy
  --workspace --all-targets` clean, full `cargo test --workspace` green
  (87 binaries). Net 85 files, -116 lines.
- 2026-06-22 · R19 · DONE · 8991e39 · Centralized every external
  dependency-version pin into `[workspace.dependencies]` (the
  full-centralization option, matching the established workspace
  convention, which already routes single-consumer deps like `arboard`/
  `clap`/`notify` through the table). Removed the dead `async-stream`
  from both consumers (`openai-sdk`, `aj-models`) and from the workspace
  table, since `rg async_stream` finds no code reference anywhere (both
  SDKs stream via `eventsource-stream`). Moved `aj-tui`'s 12 inline pins
  (`bitflags`, `crossterm`, `memchr`, `nucleo`, `nucleo-matcher`,
  `syntect`, `tokio-stream`, `unicode-segmentation`, `unicode-width`,
  `pretty_assertions`, `serial_test`, `vt100-ctt`), plus `aj-tui-testkit`'s
  `tokio-stream`/`vt100-ctt`, `aj`'s `serial_test`, and `aj-models`'s
  inline `async-trait` (a workspace entry already existed) to
  `{ workspace = true }`, carrying features verbatim. Note three of these
  (`tokio-stream`, `vt100-ctt`, `serial_test`) had become genuinely
  multi-consumer since R12's testkit extraction, so they could already
  drift. Versions preserved exactly: the only `Cargo.lock` change is the
  `async-stream`/`async-stream-impl` removal. Pure manifest change, no
  code or behavior impact. `cargo check --workspace` resolves; `cargo
  test -p aj-tui -p aj-tui-testkit` green (covers the moved dev-deps);
  `fmt` clean.
- 2026-06-23 · R20 · DONE · 8a25077 · Two batched consistency nits, no
  behavior change. (a) Truncation naming: in the live code only one of the
  three truncation impls was actually bare `truncate` (the others already
  carry contract-naming suffixes, `truncate_head`/`truncate_tail` in
  `aj-tools` and `truncate_to_width` in `aj-tui`). Renamed
  `aj-models/src/transform.rs::truncate` to `truncate_bytes`, corrected its
  doc (it slices *bytes*, not "characters on UTF-8 boundaries"), and added a
  `debug_assert!(s.is_ascii())` so the load-bearing post-`sanitize` ASCII
  invariant (the only thing keeping the byte slice from panicking) is
  enforced, not just narrated. (b) Tab-width constant: lifted the
  "tab = 3" magic into `aj-tui`'s `ansi` (the width authority) as
  `pub(crate) const TAB_WIDTH: usize = 3` and `TAB_AS_SPACES: &str = "   "`,
  tied together by a `const _: () = assert!(TAB_AS_SPACES.len() ==
  TAB_WIDTH)`. The three tab-expanding scanners (`visible_width`,
  `truncate_fragment_to_width`, `truncate_to_width`) and `break_long_word`
  now reference the constant; both component-local `TAB_AS_SPACES` copies in
  `text.rs`/`markdown.rs` are gone, retiring the convention-by-comment ("matches
  the `Text` component's constant"). Documented the genuine fork the T2
  finding flagged: `grapheme_width` returns 0 for `\t` (and all control
  chars) while `visible_width` expands it, and the compositing scanners
  (`slice_with_width`/`extract_segments`) measure via `grapheme_width`
  because they run on already-normalized rendered lines. Considered, then
  rejected, reconciling that divergence by making `grapheme_width` expand
  `\t` (which would let `break_long_word` drop its `visible_width`
  workaround): changing a width primitive's contract exceeds R20's
  mechanical scope, so the finding's documentation-only ask is the right
  fit. Tests:
  `truncate_bytes_caps_at_byte_limit`; existing ansi tab/width suite
  (`test_visible_width_tab` et al.) unchanged and green; the static assert
  is a compile-time check. `fmt`/`check`/`clippy --all-targets` clean on
  `aj-models`/`aj-tui`, `cargo check --workspace` resolves.
- 2026-06-23 · R21 · DONE · 2411abf · Aligned the keybinding
  vocabulary's two halves (the finding's first option). The canonical
  matcher `keys::parse_key_id` accepts only `ctrl`/`alt`/`shift`/`super`
  and fails closed on anything else, and its sibling
  `format_key_descriptor` deliberately refuses to emit `meta`/`hyper`
  (documented at the call site), so `super` is the one canonical
  "windows/command/meta" modifier. But the display helper
  `keybindings::format_key_segment` mapped `"meta" | "cmd" | "super" =>
  "Meta"`, advertising `meta`/`cmd` as valid input even though a
  `cmd+k` descriptor never matches a keystroke (and rendering the shared
  modifier under a different label than its canonical token). Dropped the
  `"meta"`/`"cmd"` arm and mapped `"super" => "Super"` so the label
  mirrors the canonical token; unrecognized spellings now title-case
  through the fallback like any other unknown segment (`cmd+k` →
  `Cmd+K`), so help text no longer pretends a rejected binding is valid.
  Latent contract fix, not a live regression: production only installs
  the built-in defaults (none use `super`/`meta`/`cmd`) and no
  user-override config path is wired yet, so no current display changes.
  Rejected the alias route (add `meta`/`cmd` to `parse_key_id`): it
  softens the fail-closed contract the audit praises and contradicts
  `format_key_descriptor`'s documented intent to exclude `meta`/`hyper`,
  turning an S cleanup into a vocabulary redesign over crossterm's
  separate `META`/`SUPER` modifiers. Tests: `super+k` → `Super+K`, and a
  regression asserting `cmd+k`/`meta+k` no longer format as a recognized
  modifier (so re-introducing the alias must touch both halves); the
  existing `unknown_modifier_names_reject_the_match` in `tests/keys.rs`
  already pins the parser rejection. `fmt`/`check`/`clippy --all-targets`
  clean on `aj-tui`; `cargo test -p aj-tui --lib keybindings` green.
- 2026-06-23 · RESIDUAL(anthropic-sdk) · DONE · f153563 · First
  per-crate residual sweep. The report's Major (two error entry points
  disagree) and two of its Minors (`ParseError` never constructed, the
  `messages()` keep-or-drop call) were already retired by R11, so the
  live leftovers were one Boundaries Minor, one Contracts Minor, and
  three Nits. On the Boundaries fork (prune the unused
  setters/conversions/`apply_delta` vs. document them as intentional) the
  user chose **keep everything** since we may drive the SDK for other
  things later, so nothing was pruned. Instead the `lib.rs` module doc now
  states the surface tracks the wire API rather than only the current
  consumer, naming the currently-uncalled items so a future reader can
  tell "public on purpose" from "leaked" (the finding's documentation
  option). The four nits applied: documented that `messages_stream` drops
  a mid-stream transport error as a silent end-of-stream (the consumer
  must detect a truncated turn itself, which `aj-models` does); removed
  the redundant bare `use reqwest;`; moved `ApiError`'s per-variant
  messages onto `#[error("…")]` attributes and deleted the hand-written
  `impl Display` (+ its now-unused import), keeping the strings
  byte-identical since `ClientError` composes them; and recorded the
  `CLAUDE_CODE_VERSION` failure mode (server may reject a stale value as
  an unrecognized client, OAuth-only) so a maintainer knows when to bump
  it. Tests: a new `api_error_display_strings_are_stable` pins the #4
  string preservation; `cargo test -p anthropic-sdk` green (20),
  `fmt`/`clippy -p anthropic-sdk --all-targets` clean, `cargo check
  --workspace` confirms no pruned item (none were) and no consumer drift.
- 2026-06-23 · RESIDUAL(openai-sdk) · DONE · 9b6b303 · Second per-crate
  residual sweep. Three findings were already retired and were
  verified-only: the Major `anyhow` non-streaming split and the non-2xx
  error-mapping duplication by R11 (`chat_completions`/`responses` return
  `ClientError`, the shared `classify_error_response`/`retry_after_header`
  feed all five paths, `anyhow` is gone from `Cargo.toml`), and the unused
  `async-stream` dep by R19. Four live leftovers addressed. (1)
  [Minor][Simplicity] dead surface (confirmed still unused workspace-wide:
  the two non-stream methods, `base_url`, `Response::output_text`, the
  `ResponseInstructions` `From` impls, and ~6 convenience constructors):
  **kept**, applying the anthropic-sdk precedent (surface tracks the wire
  API, not just AJ's calls) rather than pruning, with the `lib.rs` module
  doc now naming the currently-uncalled items so "public on purpose" reads
  apart from "leaked." (2) [Minor][Errors] the SSE-parse half R11 left
  duplicated: extracted a pure `parse_sse_event<T>(data) -> Option<Result<
  T, ClientError>>` plus a thin `parse_sse_stream<T>(response)`, so the two
  streaming `filter_map` bodies (previously byte-identical apart from the
  deserialized type) now share one implementation next to the existing
  error-mapping helpers. (3) [Minor][Contracts] documented the three stream
  channels on `parse_sse_stream` (transport `Err` → `InternalError`;
  `[DONE]` → clean end; a protocol `error` event yielded as `Ok` for the
  consumer to classify) and noted on `parse_sse_event` that `[DONE]` is the
  Chat Completions terminator, accepted harmlessly on the Responses path
  where `response.completed` is the real signal (kept defensively rather
  than dropped, now a single shared check). (4) [Nit][Comments] replaced
  `ApiError`'s hand-written `Display` with `#[error("OpenAI API error:
  {message}")]` (message byte-identical, dropped the unused `Display`
  import). (5) [Nit][Style] converged the section-comment style: removed
  the 38 full-width `// ---` banner rules in `responses.rs`, keeping the
  bare labels to match `chat_completions.rs` and our anti-decoration
  guidance. No behavior change on the live paths; no public-API or on-disk
  change (surface kept). Tests: `parse_sse_event` (`[DONE]`/valid/garbage)
  and an `api_error_display_is_stable` string pin; `cargo test -p
  openai-sdk` green (17), `fmt`/`clippy -p openai-sdk --all-targets` clean,
  `cargo check -p aj-models` confirms no consumer drift.
- 2026-06-23 · RESIDUAL(aj-models-core) · DONE · 7332eda · Third
  per-crate residual sweep. Two findings were already retired and
  verified-only: the unused `async-stream` dep (R19) and `refresh.rs`'s
  `anyhow` (R11, now a typed `RefreshError`). The live Major was
  `tools::Tool` (`name`/`description`/`input_schema`/`r#type`), a wire-layer
  struct that no provider read, existing only so `aj-agent` could map
  `ErasedToolDefinition → Tool → ToolDefinition` while always setting (and
  never reading) `r#type: None`. Checked the reference against the user's
  steer: its wire layer (`packages/ai`) has exactly one tool type,
  `Tool { name, description, parameters }`, with no `type` field, and its
  runtime tool (`AgentTool extends Tool`) is passed straight into the
  inference `Context` with no intermediary. AJ's `ErasedToolDefinition` is
  the analog of `AgentTool`, so `tools::Tool` was an extraneous third type.
  Deleted `aj-models/src/tools.rs` + `pub mod tools`, and `aj-agent` now
  projects `ErasedToolDefinition` straight onto `types::ToolDefinition`
  (the agent field becomes `Vec<ToolDefinition>`, so the inference site is
  a plain `self.tools.clone()`), dropping the dead `r#type` field. Kept the
  runtime's `input_schema` naming (vs the reference's uniform `parameters`)
  out of scope: unifying it touches every tool impl and the public
  `ToolDefinition` trait, beyond this finding; noted the divergence for the
  user. Minors/Nits applied: repointed the two intra-doc links from
  `crate::errors::*` to `crate::types::{AssistantError,ErrorCategory::Auth}`
  (both live in `types`), and a review-flagged adjacent bare `[`Auth`]`
  link in the same block now resolves to `ErrorCategory::Auth`;
  documented `parse_retry_after`'s past-HTTP-date →
  `Some(0)` behavior; reworded `provider.rs`'s "until a provider is
  wired/lands" module doc + two test comments to steady-state. The
  command-name Nit is ACCEPTED-AS-IS in-crate: the registry warning and
  `refresh.rs` docs already say `aj update-models`, matching the real
  `UpdateModels` clap subcommand and CLAUDE.md; the one wrong straggler
  (`aj models update` in `src/aj/src/model.rs:46`) belongs to the later
  `aj` binary sweep. No behavior change, no on-disk/wire change (the wire
  `ToolDefinition` shape is unchanged). Tests: existing `aj-models` +
  `aj-agent` suites green (the construct→infer path now uses
  `ToolDefinition` end to end); `fmt`/`clippy -p aj-models -p aj-agent
  --all-targets` clean; `cargo check --workspace` confirms no consumer
  drift.
- 2026-06-23 · RESIDUAL(aj-models-streaming) · DONE · 41267ce · Fourth
  per-crate residual sweep. Two findings were already retired and
  verified-only: the `truncate` doc nit (R20 renamed it `truncate_bytes`,
  fixed the doc, added the `is_ascii` debug-assert) and the unused
  `async-stream` dep (R19). Five live leftovers, plus the over-broad
  public-surface theme. **(1) [Minor][Boundaries] `select_cancel`/
  `SelectOutcome` cohesion:** the streaming-protocol module also housed a
  generic `select!`-vs-`CancellationToken` helper used by all four
  providers and `scripted.rs`. Moved both to a new `pub(crate)` `cancel`
  module (user-approved destination over co-locating in `types.rs`),
  restoring `streaming.rs` to just the event protocol; the five importers
  switched to `use crate::cancel::...`. No cross-crate user exists, so the
  move tightens to `pub(crate)`. **(2) [Minor][Misc] `repair_json`
  per-delta alloc:** rewrote it over `char_indices().peekable()` with
  byte-offset lookahead for the `\uXXXX` case, dropping the
  `input.chars().collect::<Vec<char>>()` it did on every non-strict-parse
  delta of the streamed tool-call hot path. No behavior change (the 4-hex
  guard is mirrored exactly). Declined the caller-side O(n²) reparse fix:
  it changes the snapshot cadence across all four provider accumulation
  loops, a design change beyond a residual cleanup. **(3) [Minor]
  [Contracts] collapse-to-`{}`:** documented on `parse_streaming_json`
  that a buffer ending in a partial number/keyword recovers no value for
  that one delta (dropping already-complete sibling keys until it
  completes); did not add the optional trailing-token trim (behavior
  change, out of scope). **(4) [Minor][Comments] `signatures_portable`
  unanchored claim:** added a focused test pinning that an
  `openai-responses` source demotes/drops thinking across a model-id
  change (`signatures_portable` false off-Anthropic even when the api
  matches). Kept the prose, which honestly records how the claim was
  established, and did **not** add a `§8.1` spec citation (R18 stripped
  those). **(5) [Nit][Style] strategy-chain comment duplication:** trimmed
  the body's numbered inline comments to just the non-obvious notes (the
  "skip the repaired parse when `repair_json` was a no-op" optimization
  and the "both lexical and structural damage" step), letting the function
  doc be the canonical chain. **Over-broad surface:** narrowed
  `partial_json` to `pub(crate) mod` with `pub(crate) fn
  parse_streaming_json` (cross-module, used by the providers) and private
  `repair_json`/`complete_partial_json` (in-module only; the `#[cfg(test)]`
  suite reaches them). The `[Nit][Comments]` per-event clone rationale got
  the cross-reference the finding asked for: a NOTE on
  `AssistantMessageEvent` that the growing-snapshot clone and the
  cumulative reparse are the two compounding per-delta costs. The
  `close_pending` wall-clock `Utc::now()` is a synthesis note, not a
  numbered finding, and is ACCEPTED-AS-IS (injecting a clock for synthetic
  orphan timestamps exceeds this sweep). Public-API change is a pure
  narrowing with no external consumers, so it can't break callers; no
  on-disk/wire change. Tests: partial-number/keyword collapse, a
  truncated-`\u`-at-end + multibyte adversarial pair pinning the no-Vec
  rewrite, and the `openai-responses` demotion. `cargo test -p aj-models`
  green (60 roundtrip + 319 lib), `fmt`/`clippy -p aj-models --all-targets`
  clean, `cargo check --workspace` confirms no consumer drift.
- 2026-06-23 · RESIDUAL(aj-models-anthropic) · DONE · 32545fa · Fifth
  per-crate residual sweep. Two findings were verify-only (already retired
  by themed work): the Major truncated-stream-as-`Done` (R1's
  `finalize_or_truncate` emits a retryable `Transient` error when no
  terminal frame arrived) and the happy-path-only roundtrip Testing Minor
  (R16's `truncated`/`error_frame`/`refusal` fixtures). Live changes, all in
  `anthropic/provider.rs` except the cross-provider feature gate.
  **[Minor][Boundaries] over-broad public surface for tests (the
  cross-provider theme):** the per-provider round-trip helpers
  (`replay_sse_events` + the request/response item projections) were `pub`
  solely so the `tests/roundtrip/` integration crate (a separate
  compilation unit) could reach them. A shared crate is a non-starter here
  because `replay_sse_events` drives the private `StreamState`, so it can
  only live in the provider module. Resolved (user call: option B over a
  lighter `#[doc(hidden)]`) with a new default-off `test-support` cargo
  feature on `aj-models`, gating each helper `#[cfg(any(test, feature =
  "test-support"))]`, plus a self dev-dependency (`aj-models = { path =
  ".", features = ["test-support"] }`) that turns the feature on for our own
  test targets so `cargo test` works without `--features`. So the helpers
  vanish from a production build entirely (verified: `cargo build
  --workspace` and `cargo check -p aj-models` compile clean with them
  absent), matching the `aj-tools::testing` precedent. Applied to **all
  four providers** at the user's request (anthropic + the three OpenAI
  adapters), so it also retires the round-trip-helper portion of the
  `aj-models-openai` report's equivalent pub-for-tests finding. That
  finding also names the `TextSignatureV1` envelope, a struct used in
  production (so it can't be feature-gated, only visibility-narrowed) and
  also reachable by the test crate, so it's a different sub-case left for
  the `aj-models-openai` sweep. Gating exposed that the reverse-parse
  path (`parse_assistant_input_items_with_api`, `push_message_text`, and the
  codex `append_assistant_message`/`parse_assistant_input_items_with_api`/
  `ServiceTier` imports) is itself entirely test-only (the live provider
  parses streamed *responses*, never request items back), so those were
  gated/split out too, keeping both the feature-on and feature-off builds
  warning-free. **[Minor][Contracts] reachable `expect` on the wire
  `index`:** replaced the three `usize::try_from(index).expect(...)` in
  `process` with the file's own defensive early-return (`let Ok(idx) = ...
  else { return ProcessOutcome { events, terminal } }`). Honest note: the
  wire type is `u64`, so on 64-bit `try_from` is infallible and the new
  branch only guards 32-bit targets. The change removes a
  panic-in-principle and matches the parser's otherwise-total handling, but
  is not 64-bit-observable. **[Minor][Comments]** deleted the duplicated `finalize`
  comment paragraph (kept the structured-error wording, dropped the
  em-dash). **[Nit][Comments]** clarified the `MessageStart` "trust the
  wire value" comment (display-only: cost keys off the registry `ModelInfo`,
  not `partial.model`). **[Nit][Simplicity]** simplified `replay_sse_events`
  (anthropic), dropping the `last_event` accumulator and the unreachable
  `panic!` arm: capture the terminal `Error` inside the loop and finalize via
  `finalize_or_truncate().partial().clone()` (total over `Done`/`Error`).
  **[Nit, from the boundary notes]** added a rationale comment to the
  `PauseTurn → Stop` arm (we run tools in the agent loop, not server-side,
  so pause-turn doesn't arise in practice, and a completed-turn mapping is
  the safe default, with no behavior change). The OpenAI
  adapters' own non-boundary leftovers (e.g. their `replay_sse_events`
  panic arm) stay for the `aj-models-openai` sweep. Tests: a new
  `streamstate_out_of_range_index_drops_block_without_panicking` pins the
  defensive index path. The existing `streamstate_finalize_maps_stop_reasons`
  covers PauseTurn→Stop and the roundtrip suite drives the simplified
  `replay_sse_events` (the error/truncation legs included). `cargo test
  -p aj-models` green (320 lib + 60 roundtrip), `fmt`/`clippy -p aj-models
  --all-targets` clean, `cargo check --workspace --all-targets` confirms no
  consumer drift. The non-leak is established by `cargo build --workspace`
  / `cargo check -p aj-models` (dev-deps inactive there, so the feature is
  off and the helpers are absent), not by the `--all-targets` run, which
  activates the dev-edge and turns the feature on.
- 2026-06-23 · RESIDUAL(aj-models-openai) · DONE · bfca37c · Sixth
  per-crate residual sweep. Four findings were verify-only (retired by
  themed work): both Majors (stream-end-without-terminal → R1's
  `finalize_or_truncate`; the thrice-copied `classify_client_error` →
  R10's `openai::errors`), the happy-path-only roundtrips (R16's
  error/truncation fixtures), and the round-trip-helper half of the
  pub-for-tests Boundaries finding (the four-provider `test-support`
  gate from the anthropic sweep). Scouted `/tmp/pi` for each live item
  and it reshaped two calls. **[Minor][Errors] Codex divergent
  terminal-error handling (reconciled, the one behavior change):** Codex
  short-circuited `error`/`response.failed` to an `Err` and rebuilt a
  *fresh empty* `AssistantMessage`, dropping the streamed partial, where
  the Responses provider forwards the same events into the shared
  `StreamState` and `finalize` keeps the partial. The reference confirms
  both of its providers preserve the partial on a terminal error (throw
  caught into the same accumulated `output`), so our drop diverged from
  both our own Responses adapter and the reference. Made
  `normalize_codex_event` forward `Error`/`ResponseFailed` as `Terminal`
  into the state machine (so `finalize` emits an `Error` carrying the
  partial + classified category, identical to Responses), which makes
  the function infallible: dropped its `Result`, the `?` in the run
  loop, the `replay_sse_events` `Err` arm, and the now-unused
  `error_from_code` import. The friendly-429 overlay is HTTP-level
  (`classify_codex_client_error`, unchanged); in-stream error
  classification was already `error_from_code`/`error_from_response`, so
  the only message-string change is the bare-failure fallback prefix.
  **[Minor][Boundaries] `TextSignatureV1` pub-for-tests (kept +
  documented, reversing the initial plan):** the reference keeps the
  envelope *type* public (part of the shared message model) and the
  codec private; our codec is already `pub(super)`, and `types.rs`'s
  `text_signature` field doc already names the type as its encoding. So
  the type stays `pub` with a doc note that it's the documented wire
  encoding, not a test-only leak (matching the SDK residual-sweep
  precedent), rather than narrowing it behind a gated seam. **[Minor]
  [Contracts] dead `ProcessOutcome.terminal`:** the field was hardcoded
  `false` so the `if outcome.terminal { break }` was unreachable (R1
  fixed the `saw_terminal` method but left this). Collapsed `process` to
  return `Vec<AssistantMessageEvent>` (aligning Completions with
  Responses/Codex) and end the loop on stream close, matching the
  reference's no-terminal-flag loop. **[Minor][Comments]** rewrote the
  stale `finalize` close-block comment (it narrated a synthetic-`Done`
  branch that doesn't exist; post-R1 the call is a defensive no-op).
  **[Nit][Testing]** factored the pure `minutes_until_at(resets_at,
  now_secs)` out of the wall-clock `minutes_until` and tested the
  rounding deterministically (the reference reads `Date.now()` inline, so
  this exceeds it). **[Nit][Simplicity]** inlined + dropped the typed
  `normalize_response_status` no-op (the SDK `ResponseStatus` has no
  catch-all, so a typed status is always recognized; the real
  normalization is the untyped `normalize_response_status_in_value`,
  kept, which is the analog of the reference's `normalizeCodexStatus`).
  **[Nit][Comments] tool-call index invariant (the user asked for a
  proper fix, not just a doc):** the reference defends a shifting/late
  wire index with a secondary id-keyed map, so beyond documenting the
  relied-upon invariant we added a `tool_calls_by_id` fallback: when a
  delta's wire index misses an open slot but its tool-call `id` matches a
  seen call, we reuse that slot instead of splitting one logical call.
  (A backend reusing one index for distinct calls would still merge them;
  noted in the field doc.) No public-API or on-disk change (the type
  stays `pub`); the one behavior change preserves strictly more data on a
  failed Codex turn. Tests: Codex terminal-error forwarding +
  partial-preserving roundtrip (flipped from asserting the drop), the
  `minutes_until_at` rounding boundaries, and a shifted-index tool-call
  that must not split. `cargo test -p aj-models` green (324 lib + 60
  roundtrip), `fmt`/`clippy -p aj-models --all-targets` clean, `cargo
  check --workspace` + `cargo build -p aj-models` confirm no consumer
  drift and that the gated helpers stay absent from a prod build. A
  fresh-agent review then flagged the tool-call id fallback's ordering
  and suggested id-first resolution (index-first can route a
  shifted-index continuation onto an occupied sibling slot). On a
  closer read of the reference its resolution is index-first with an
  id-fallback (the wire index is the authoritative, always-present key
  and the id arrives only on a call's first delta, so it serves as a
  recovery fallback when the index misses). We kept that design rather
  than the review's id-first, since id-first would diverge from the
  reference and trade one pathological-backend case for another, and
  documented the deliberate limitation on the field. The review's other
  fixes were applied: corrected stale doc/comment wording the reconcile
  left behind (the codex module/enum docs, the `replay_sse_events` doc,
  the `TextSignatureV1` codec-visibility note, and two comment
  semicolons), and kept the shift-to-fresh-index fallback regression
  test (the case index-first does recover).
- 2026-06-24 · RESIDUAL(aj-models-auth) · DONE · 044e142 · Seventh
  per-crate residual sweep. Four findings were verify-only, already
  retired by themed work: the Major token-body leak (R4's
  `redacted_body_summary` in `token::send_token_request`), the Major
  ~80% OAuth duplication (R10's `callback`/`paste`/`token`/`test_support`
  seam), the scripted-surface Boundaries Minor (R12 kept `scripted` in
  prod, documented `ExhaustedBehavior::Panic` test-only), and the
  timeout-vs-margin Contracts Minor (R10's NOTE on
  `REFRESH_SAFETY_MARGIN_MS` relating it to `LOCK_TIMEOUT +
  REQUEST_TIMEOUT_SECS`). The user asked to cross-check the live items
  against the reference before implementing, which reshaped two of them.
  **One behavior change (finding 4(1), the key cross-check learning):** a
  stored OAuth credential whose provider id isn't in the registry (a
  hand-edited or renamed id) previously hard-errored `UnknownProvider`
  via `?` even for a fresh token. It now resolves to `Ok(None)`, matching
  the reference's `getApiKey`, which returns `undefined` for an unknown
  OAuth provider so the host shows its "log in from the palette" message
  (the `Ok(None)` arm in `model.rs`) rather than a raw error. We can
  neither validate nor refresh such a credential, so treating it as
  unconfigured is both friendlier and the production-tested behavior. The
  fix is a `let-else` early return that swallows only `UnknownProvider`
  (the lone error `lookup_oauth_provider` returns). `login()` still errors
  on an unknown provider, and the refresh-failure path still bubbles
  `AuthError::OAuth` (our deliberate, documented design, arguably better
  than the reference's collapse-to-none since a transient refresh blip
  keeps its cause). Chose this over the proposal's original
  "document-as-fatal" once the reference showed the gentler path is
  validated in production. **Finding 4(2):** documented on `get_api_key`
  that env-supplied OAuth tokens (`ANTHROPIC_OAUTH_TOKEN`,
  `OPENAI_CODEX_OAUTH_TOKEN`) are returned without refresh, so a stale one
  surfaces as a 401. **Finding 7 (`extra` flatten):** doc-only, correcting
  the finding's premise. The reference's `OAuthCredentials` is the same
  `[key: string]: unknown` catch-all with no guard, but our Rust core
  fields are *required* (no `serde(default)`), so a renamed core field is
  a loud parse error, not a silent empty `access`. Documented that, did
  not add the suggested non-empty assertion (a missing field already
  errors). **Finding 8:** replaced `generate_state`'s per-byte
  `push_str(&format!(...))` with `write!` into the pre-sized buffer
  (byte-identical output), matching the reference's one-shot hex encode.
  **Finding 5/F10 (`MAX_REQUEST_HEAD_BYTES`):** softened the doc to state
  we parse-what-we-have on hitting the cap rather than reject (the request
  line always precedes headers, and an over-cap request line just fails to
  parse → 400 → keep listening). The reference delegates to Node's HTTP
  server, so there was no manual reader to mine. **Finding 11
  (clock dedup + comment):** folded the last two `Utc::now()` readers
  (`auth::current_unix_ms`, `oauth::token::now_unix_ms`) into one
  `pub(crate) oauth::now_unix_ms`, next to `is_expired_at` (its semantic
  home), updating all four callers, and dropped the misleading "so tests
  can stub it" comment (the real seam is `is_expired_at(now)`). **Finding
  9:** ACCEPTED-AS-IS, the `find_env_keys` doc already reads "four
  providers" with the anthropic preference order and the codex
  no-refresh note, exactly what the finding asked. **Tests (finding 5):**
  `get_api_key_surfaces_refresh_failure_and_keeps_stale_cred` (refresh
  `Err` surfaces `AuthError::OAuth` and leaves the stale cred on disk),
  `get_api_key_unknown_oauth_provider_resolves_to_none` (locks the
  behavior change), and two `obtain_code_and_state` tests driving the
  `biased` `select!` manual arm through the state-mismatch reject and the
  empty-input-then-prompt fallback (the listener never receives a
  connection, so `accept` stays pending and the ready manual-input future
  wins deterministically). **Noted, not changed:** the reference checks
  env *after* stored credentials (`runtime > stored > env > fallback`),
  the inverse of our `runtime > env > stored` order. A background research
  pass confirmed the reference's ordering is deliberate and documented
  (it protects a `/login`'d subscription from being shadowed and
  mis-billed by an ambient `ANTHROPIC_API_KEY`, recovering the override
  use case via `--api-key` and `auth.json` env interpolation). Ours
  optimizes for env-as-quick-override and is a documented deliberate
  choice, so left as-is and flagged for a possible future call. Also
  noted a pre-existing divergence the reviewer caught: `aj::auth::
  provider_status` reads the raw stored credential, so a typo'd-id OAuth
  entry shows "configured" there while `get_api_key` now returns `None`.
  Not introduced here, only surfaces in the degenerate typo state, left
  for awareness. A fresh-agent review confirmed the manual-paste tests are
  race-free, the behavior change is handled by all three `get_api_key`
  callers (`model.rs`, `usage.rs` x2), the `let-else` masks no other
  error, and the docs are accurate; its two doc nits (a semicolon and an
  overstated cap claim) were fixed. `cargo test -p aj-models --lib` green
  (329, +4), `fmt`/`clippy -p aj-models --all-targets` clean, `cargo check
  --workspace` confirms no consumer drift.
- 2026-06-24 · FOLLOWUP(aj-models-auth) · DONE · ae81600 · Actioned the
  "possible future call" flagged in the entry above: flipped our API-key
  resolution order from `runtime > env > stored` to `runtime > stored API
  key > stored OAuth > env`, adopting the reference's model so a deliberate
  `aj login` or hand-edited `auth.json` credential stays authoritative and
  a stray exported key (e.g. an `ANTHROPIC_API_KEY` in a shell profile)
  can't silently shadow and mis-bill against it. Env becomes the
  lowest-priority fallback. The runtime `--api-key` flag (still rank 1)
  remains the explicit per-run override, which is where the old
  "env-as-quick-override" use case now lives. Matched the reference's
  structure exactly except for our one deliberate divergence (a refresh
  failure bubbles `AuthError::OAuth` instead of falling through to env). An
  unknown-provider stored OAuth still short-circuits to `Ok(None)` with no
  env fallback (faithful to the reference), and a sibling-cleared entry
  under the lock now falls through to env. Reordered the binary's
  `aj::auth::provider_status` overlay to mirror the chain (stored reported
  before env). Updated every doc that stated the order: the `auth.rs`
  module + `get_api_key` docs, `model.rs`'s `ApiKeyResolver` module doc,
  `provider_status`/`ProviderAuthStatus` docs, `models-spec.md` §9.1 (list
  + rationale) and §9.5 (env-mapping note, plus added the missing
  `openrouter` row), and `openrouter-spec.md`. New `#[serial]` test
  `get_api_key_stored_credential_beats_env_var` pins both directions (env
  as fallback when nothing stored, stored wins when present) via an
  `EnvVarGuard` RAII helper. Added `serial_test` as an `aj-models`
  dev-dependency. The pre-existing `provider_status`-over-reports-typo'd-id
  divergence noted above is unchanged. A fresh-agent review found no
  correctness defects and flagged the two stale doc spots (`model.rs`,
  `openrouter-spec.md`) and an overstated `EnvVarGuard` SAFETY comment, all
  fixed. `cargo test -p aj-models --lib` (330, +1) and `-p aj --lib` (450)
  green, `clippy -p aj-models -p aj --all-targets` clean, `cargo check
  --workspace` clean.
- 2026-06-24 · RESIDUAL(aj-conf) · DONE · a116f88 · Eighth per-crate
  residual sweep. Two findings verify-only, retired by R5: the Major
  non-atomic `Config::save` (now the lock + read-merge-write
  `persist_changed`, and the user-confirmed reframe that temp+rename/fsync
  is over-engineering vs. the reference's plain-write-under-lock) and the
  Minor "no clobber-guard test" (`persist_changed_refuses_to_clobber_
  invalid_toml` + `_creates_a_missing_file` exist). The user asked to
  cross-check the live items against the reference first, which confirmed
  every call. The reference splits these exact concerns across separate
  files (`config.ts` paths, `settings-manager.ts` schema+lock+merge,
  `resolve-config-value.ts` resolution, `resource-loader.ts` context
  loading), funnels `$HOME` through one `homedir()` + `getAgentDir()`,
  injects `cwd`/`agentDir` into its loader for hermetic tests, and stores
  sessions in one flat dir (sidestepping any path-derived name collision).
  Acted on each live finding. **[Major][Boundaries] split the 2704-line
  `lib.rs`** into `schema` (the `config.toml` schema, parser, typed
  diagnostics, and lock-guarded writer), `paths` (`$HOME` resolution, the
  `~/.aj/` resolvers, git-root + project-dir discovery, display
  abbreviation), and `env` (`AgentEnv` + context-file loading), with a
  re-export-only `lib.rs`. Public API is unchanged: types re-export from the
  crate root and the path resolvers stay `impl Config` methods (Rust allows
  a type's inherent impl to span files), so `Config::get_config_dir()` and
  friends keep their paths and no consumer call site moved. The verbatim
  move was done first and proven green before any semantic edit.
  **[Minor][Testing] injectable `AgentEnv`:** added `AgentEnv::discover(
  working_directory, home, today_date, ...)` as the hermetic core with
  `new()` the thin real-host wrapper that reads cwd/`$HOME`/clock, matching
  the reference's injected-`cwd`/`agentDir` loader; rewrote the two
  `new()`-based smoke tests into a hermetic `discover_is_hermetic`, a
  documented `new_reads_the_host_environment` smoke test, and a deterministic
  `display_format_is_stable`. **[Minor][Simplicity] `$HOME`:** one
  `paths::home_dir() -> Option<PathBuf>` now backs all six prior scattered
  reads (config dir, display, dir-name, system prompt, user instructions,
  skills), each adapting the `None` case to its own policy; the doc records
  that contract. **[Minor][Contracts] `path_to_dir_name` collision:**
  documented the lossy dash-join + outside-`$HOME` fallback on both
  `path_to_dir_name` and `get_sessions_dir_path` rather than changing the
  scheme (which would relocate every existing project's sessions dir); the
  reference avoids it structurally with flat storage, out of scope for a
  residual sweep. **[Minor][Contracts] precedence doc:** reworded the
  `Config` doc to say the struct is the file layer only and the
  CLI>env>config merge lives in `aj::model`. **[Nit] `ConfigThinkingDisplay`
  doc:** collapsed the duplicated half-written opening into one summary +
  the single-knob framing + table. **[Nit] `\u2014`:** the literal escape in
  `get_sessions_base_dir_path`'s doc became two sentences (no em dash, per
  our style). **Dependency hygiene:** dropped the unused chrono `serde`
  feature (chrono is used only for `Utc::now().format`); other crates still
  enable it where needed. **ACCEPTED-AS-IS** the enum
  `Display`/`FromStr`/serde triple (now four enums: thinking level/display,
  speed, verbosity): the reference's string-union/zod idiom doesn't carry to
  Rust, a macro would add indirection for low payoff, and the
  `test_options_table_*` drift tests compensate. Left three pre-existing
  private-intra-doc-link warnings (`ConfigError::LockTimeout` →
  `LOCK_ACQUIRE_TIMEOUT`, `persist_changed` → `ConfigLock`, `skills` →
  `skill_roots`) as-is, outside the findings; introduced no new ones. No
  public-API or on-disk/wire change. `cargo test -p aj-conf` green (74),
  `fmt`/`clippy -p aj-conf --all-targets` clean, `cargo build --workspace`
  confirms no consumer drift.
- 2026-06-25 · RESIDUAL(aj-agent-runtime) · DONE · acf7a42 · Ninth
  per-crate residual sweep. Of the AG1 report's 1 Critical / 3 Major / 4
  Minor / 3 Nit, nearly all were verify-only, already retired by themed
  work: the Critical `aj-agent → aj-conf` edge + the edition/version pin
  (R7), the Major truncated-`Done`-trusted-blindly (R1), the Major three
  never-emitted event variants (R13 wired `ToolExecutionUpdate`, R14
  wired `TurnEnd` + found `QueueUpdate` live), the Major `tools::Tool ↔
  ToolDefinition` round-trip (aj-models-core sweep deleted `tools.rs`),
  the Minor `anyhow`-in-the-turn-API (R11's `BoxError`/`TurnError`), the
  Minor always-empty `AgentEnd.messages` (R14 populated it), and the Nit
  comment chronology (R18). The Nit `determine_thinking` doc/ordering is
  moot: the thinking trigger-word mechanism was removed wholesale, so
  thinking is now a fixed `ThinkingConfig` policy. Two live leftovers,
  both cross-checked against the reference per the user. **(1)
  [Minor][Contracts] reachable `expect`s on `assembled_system_prompt`:**
  the public turn path could panic if a library consumer drove a turn
  before `seed_session`. The reference sidesteps this entirely
  (`agent.ts`: `systemPrompt: initialState?.systemPrompt ?? ""`, a
  non-optional string, and every provider tolerates an empty system
  prompt), and our wire layer already matches (anthropic `build_system`
  returns `None` on empty, the OpenAI builders gate on `&& !is_empty`,
  codex substitutes `"You are a helpful assistant."`). So instead of the
  proposal's typed-error boundary guard, adopted the reference's design:
  changed `assembled_system_prompt` from `Option<String>` to a plain
  `String` (default `""`), dropped both `expect`s, and pass
  `Some(prompt)` to the `Context`. An unseeded agent now degrades to a
  promptless turn rather than panicking. The accessor returns `&str`,
  `seed_session` overwrites on `Some`, and the two `#[cfg(test)]` callers
  in `session.rs` lost their `.expect()`. Net code reduction, no
  public-signature or on-disk/wire change; the behavior change is
  strictly-better handling of a previously-panicking misuse. **(2)
  [Nit][Testing] cancellation tests raced wall-clock timers:** the
  multi-tool partial-cancel branch was already covered by
  `cancellation_synthesizes_results_for_whole_batch` (added since the
  audit), but it and `cancel_mid_stream_pushes_aborted_partial...` both
  raced a background `sleep` against a long provider/tool delay. The
  reference's abort tests trigger explicitly and let a signal-responsive
  stream react, never racing a timer; mirrored that. The batch test now
  fires the cancel from inside the first probe's `execute` (new
  `cancel_on_start: Option<CancellationToken>` test field) so the
  in-flight batch is provably cancelled with no timer at all; the
  streaming-arm test (no tool to fire from) uses
  `#[tokio::test(start_paused = true)]` so tokio's virtual clock advances
  the 50ms cancel deterministically before the 60s `Done`, which also
  fixes a real (if rare) `Start`-vs-cancel ordering race the old
  wall-clock form could lose. Added `prompt_on_unseeded_agent_runs_with_
  empty_system_prompt` pinning the no-panic fallback. A fresh-agent
  review found no must-fix bugs, confirmed `Some("")` is handled
  identically to absent across all four providers and that the only
  production composition root always seeds non-empty; its should-fix
  doc-accuracy items (the codex "sends no system prompt" overstatement, a
  pre-existing structuring semicolon in the field doc, an awkward
  `AgentSeed` doc rewrite) were folded into the amend. Flagged, not done:
  the reviewer's note that anthropic/openai-completions/openai-responses
  lack an explicit `Some("") → no system block` test now that the path is
  reachable (codex has one), left for a possible aj-models follow-up to
  keep this change aj-agent-scoped. The report's `SessionState`-publicness
  boundary note is punted to the aj-agent-contracts (AG2) sweep, the next
  RESIDUAL crate. `cargo test -p aj-agent` (77) + `-p aj` session tests
  green, `fmt`/`clippy -p aj-agent -p aj --all-targets` clean, `cargo
  check --workspace` confirms no consumer drift.
